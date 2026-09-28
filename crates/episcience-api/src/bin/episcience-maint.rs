//! `episcience-maint` -- EpiScience's maintenance acts, run through the
//! narrow maintenance login.
//!
//! ```text
//! episcience-maint backfill-owners --principal <uuid> --dry-run --manifest <path> [--expect-group <uuid>]
//! episcience-maint backfill-owners --principal <uuid> --apply   --manifest <path> [--expect-group <uuid>]
//! episcience-maint backfill-owners --reverse <manifest>
//! episcience-maint tick
//! ```
//!
//! `tick` is the maintenance timer's act (every 2 minutes): the narrowing
//! sweep (5037's `episcience_maint_sweep_narrowed`), then the ALERT check
//! (5039's `episcience_maint_unpublishable_public`): a public row still
//! unpublishable right after the sweep is one the sweep could not narrow and
//! audited as `episcience.maint.sweep_blocked`. The tick names each such row
//! and exits with [`EXIT_BLOCKED`] (3), so the unit fails and its failure
//! hook alerts; an operator remedies it (RUNBOOK). Exit 0 otherwise.
//!
//! `backfill-owners` is the one-shot legacy re-own (migration 5034's
//! `episcience_maint_backfill_owners`): the legacy root rows with no owner go
//! to the EXISTING personal group of `--principal`; derived rows follow their
//! parents. The principal is always an argument, supplied by the operator at
//! deploy time. `--dry-run` changes nothing and writes the manifest of what
//! `--apply` would do; `--apply` writes the manifest of what it did (the
//! input of `--reverse`). `--expect-group` refuses unless the target group the
//! database resolves is exactly that one (checked by a dry run BEFORE an
//! apply). The manifest file is created new (never overwritten), mode 0600.
//! `--reverse` restores a manifest, only while the ownership pair is still
//! nullable (before migration 5035).
//!
//! Reads ONLY `EPISCIENCE_MAINT_DATABASE_URL` (the `episcience_maint` login)
//! and refuses to start while `DATABASE_URL`, `EPISCIENCE_MIGRATION_DATABASE_URL`,
//! `MAINTENANCE_DATABASE_URL`, `EPISCIENCE_WORKER_DATABASE_URL` or a retired
//! service-client variable (`config::RETIRED_SERVICE_VARS`) is set; refuses a superuser, BYPASSRLS or
//! kernel-maintenance session, and a role switch. No `.env` file is read. DSNs are never printed.

use std::path::PathBuf;

use episcience_db::maint;
use sqlx::Connection;
use uuid::Uuid;

use episcience_api::config::{
    MAINT_DATABASE_URL_VAR as URL_VAR, MAINT_FORBIDDEN_VARS as FORBIDDEN_VARS,
};

const USAGE: &str = "usage:\n  \
    episcience-maint backfill-owners --principal <uuid> (--dry-run | --apply) --manifest <path> [--expect-group <uuid>]\n  \
    episcience-maint backfill-owners --reverse <manifest>\n  \
    episcience-maint tick\n\
    reads EPISCIENCE_MAINT_DATABASE_URL only";

/// The tick's exit status when the sweep left rows it could not narrow (the
/// alert). Distinct from 1 (a failure) and 2 (a refusal).
const EXIT_BLOCKED: i32 = 3;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Backfill {
        principal: Uuid,
        apply: bool,
        manifest: PathBuf,
        expect_group: Option<Uuid>,
    },
    Reverse {
        manifest: PathBuf,
    },
    Tick,
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    let (first, rest) = args.split_first().ok_or_else(|| USAGE.to_string())?;
    if first == "tick" {
        return if rest.is_empty() {
            Ok(Command::Tick)
        } else {
            Err(format!("tick takes no argument\n{USAGE}"))
        };
    }
    if first != "backfill-owners" {
        return Err(USAGE.to_string());
    }
    let mut principal = None;
    let mut mode: Option<bool> = None;
    let mut manifest = None;
    let mut expect_group = None;
    let mut reverse = None;
    let mut it = rest.iter();
    let uuid = |v: Option<&String>, flag: &str| -> Result<Uuid, String> {
        v.ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?
            .parse::<Uuid>()
            .map_err(|_| format!("{flag} needs a uuid\n{USAGE}"))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--principal" => principal = Some(uuid(it.next(), "--principal")?),
            "--expect-group" => expect_group = Some(uuid(it.next(), "--expect-group")?),
            "--dry-run" | "--apply" => {
                if mode.is_some() {
                    return Err(format!("give exactly one of --dry-run / --apply\n{USAGE}"));
                }
                mode = Some(a == "--apply");
            }
            "--manifest" => {
                manifest =
                    Some(PathBuf::from(it.next().ok_or_else(|| {
                        format!("--manifest needs a path\n{USAGE}")
                    })?))
            }
            "--reverse" => {
                reverse =
                    Some(PathBuf::from(it.next().ok_or_else(|| {
                        format!("--reverse needs a manifest path\n{USAGE}")
                    })?))
            }
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    match (reverse, principal, mode, manifest, expect_group) {
        (Some(m), None, None, None, None) => Ok(Command::Reverse { manifest: m }),
        (None, Some(principal), Some(apply), Some(manifest), expect_group) => {
            Ok(Command::Backfill {
                principal,
                apply,
                manifest,
                expect_group,
            })
        }
        _ => Err(USAGE.to_string()),
    }
}

/// The DSN, or the refusal.
fn resolve_url(get: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    for v in FORBIDDEN_VARS {
        if get(v).is_some() {
            return Err(format!(
                "refusing: {v} is set. episcience-maint reads only {URL_VAR} (the maintenance login); \
                 unset {v}"
            ));
        }
    }
    match get(URL_VAR) {
        Some(u) if !u.trim().is_empty() => Ok(u),
        _ => Err(format!("{URL_VAR} is not set")),
    }
}

/// Create `path` new (never overwrite), mode 0600, with `body`.
fn write_new_private(path: &PathBuf, body: &str) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| format!("create manifest {}: {e}", path.display()))?;
    f.write_all(body.as_bytes())
        .and_then(|()| f.write_all(b"\n"))
        .map_err(|e| format!("write manifest {}: {e}", path.display()))
}

fn summary(manifest: &serde_json::Value) -> String {
    let counts = manifest["counts"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    format!(
        "target_group={} applied={} counts: {counts}",
        manifest["target_group"].as_str().unwrap_or("?"),
        manifest["applied"]
    )
}

/// The sweep, then the alert check. See the module documentation.
async fn tick(conn: &mut sqlx::PgConnection) -> i32 {
    let narrowed = match maint::sweep_narrowed(conn).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("episcience-maint: tick: sweep failed: {e}");
            return 1;
        }
    };
    let blocked = match maint::unpublishable_public(conn).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("episcience-maint: tick: narrowed {narrowed}; the blocked check failed: {e}");
            return 1;
        }
    };
    if blocked.is_empty() {
        println!("episcience-maint: tick: narrowed {narrowed}; blocked 0");
        return 0;
    }
    for (kind, id) in &blocked {
        eprintln!(
            "episcience-maint: ALERT: the sweep could not narrow {kind} {id} \
             (audited as episcience.maint.sweep_blocked; see the runbook remedy)"
        );
    }
    eprintln!(
        "episcience-maint: tick: narrowed {narrowed}; blocked {}",
        blocked.len()
    );
    EXIT_BLOCKED
}

#[tokio::main]
async fn main() {
    std::process::exit(real_main().await);
}

async fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_command(&args) {
        Ok(c) => c,
        Err(u) => {
            eprintln!("{u}");
            return 2;
        }
    };
    if let Err(e) = episcience_api::config::refuse_retired_service_vars(
        "episcience-maint",
        episcience_api::config::env_value,
    ) {
        eprintln!("episcience-maint: {e}");
        return 2;
    }
    let url = match resolve_url(episcience_api::config::env_value) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("episcience-maint: {e}");
            return 2;
        }
    };
    let mut conn = match sqlx::PgConnection::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("episcience-maint: connect: {e}");
            return 1;
        }
    };
    // The one privileged-session check every EpiScience process shares: the
    // maintenance login is deliberately narrow, so a broader DSN in its place
    // (a migration DSN pasted by mistake, or a role switch in its options) is
    // refused rather than used.
    if let Err(e) = episcience_db::tenancy_contract::refuse_privileged_session(&mut conn).await {
        eprintln!("episcience-maint: refusing: {e}; use the episcience_maint login ({URL_VAR})");
        return 2;
    }

    match cmd {
        Command::Tick => tick(&mut conn).await,
        Command::Backfill {
            principal,
            apply,
            manifest,
            expect_group,
        } => {
            if manifest.exists() {
                eprintln!(
                    "episcience-maint: {} already exists; a manifest is never overwritten",
                    manifest.display()
                );
                return 2;
            }
            if let Some(want) = expect_group {
                // A dry run resolves the target group without changing anything.
                let probe = match maint::backfill_owners(&mut conn, principal, false).await {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("episcience-maint: backfill refused: {e}");
                        return 1;
                    }
                };
                let got = probe["target_group"].as_str().unwrap_or("");
                if got != want.to_string() {
                    eprintln!(
                        "episcience-maint: refusing: the principal's personal group is {got}, not the expected {want}; nothing changed"
                    );
                    return 1;
                }
            }
            let m = match maint::backfill_owners(&mut conn, principal, apply).await {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("episcience-maint: backfill refused: {e}");
                    return 1;
                }
            };
            let body = match serde_json::to_string_pretty(&m) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("episcience-maint: serialize manifest: {e}");
                    return 1;
                }
            };
            if let Err(e) = write_new_private(&manifest, &body) {
                // The apply (if any) already committed: say so plainly.
                eprintln!(
                    "episcience-maint: {e}; the backfill {} — the manifest is below",
                    if apply {
                        "WAS APPLIED"
                    } else {
                        "was a dry run"
                    }
                );
                println!("{body}");
                return 1;
            }
            println!(
                "episcience-maint: backfill-owners {}: {} (manifest {})",
                if apply { "applied" } else { "dry run" },
                summary(&m),
                manifest.display()
            );
            0
        }
        Command::Reverse { manifest } => {
            let text = match std::fs::read_to_string(&manifest) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("episcience-maint: read {}: {e}", manifest.display());
                    return 2;
                }
            };
            let m: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("episcience-maint: {} is not JSON: {e}", manifest.display());
                    return 2;
                }
            };
            if m["kind"] != maint::MANIFEST_KIND {
                eprintln!(
                    "episcience-maint: {} is not a backfill manifest",
                    manifest.display()
                );
                return 2;
            }
            match maint::backfill_reverse(&mut conn, &m).await {
                Ok(n) => {
                    println!("episcience-maint: reverse restored {n} rows");
                    0
                }
                Err(e) => {
                    eprintln!("episcience-maint: reverse refused: {e}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    const P: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn the_backfill_needs_a_principal_a_mode_and_a_manifest() {
        assert_eq!(
            parse_command(&a(&format!(
                "backfill-owners --principal {P} --dry-run --manifest m.json"
            ))),
            Ok(Command::Backfill {
                principal: P.parse().unwrap(),
                apply: false,
                manifest: "m.json".into(),
                expect_group: None
            })
        );
        assert!(matches!(
            parse_command(&a(&format!(
                "backfill-owners --principal {P} --apply --manifest m.json --expect-group {P}"
            ))),
            Ok(Command::Backfill {
                apply: true,
                expect_group: Some(_),
                ..
            })
        ));
        for bad in [
            "backfill-owners --dry-run --manifest m.json".to_string(),
            format!("backfill-owners --principal {P} --manifest m.json"),
            format!("backfill-owners --principal {P} --dry-run --apply --manifest m.json"),
            format!("backfill-owners --principal {P} --apply"),
            "backfill-owners --principal not-a-uuid --apply --manifest m.json".to_string(),
            format!("backfill-owners --reverse m.json --principal {P}"),
            format!("backfill-owners --principal {P} --apply --manifest m.json --force"),
            format!("migrate --principal {P} --apply --manifest m.json"),
        ] {
            assert!(parse_command(&a(&bad)).is_err(), "{bad}");
        }
        assert_eq!(
            parse_command(&a("backfill-owners --reverse m.json")),
            Ok(Command::Reverse {
                manifest: "m.json".into()
            })
        );
    }

    /// `tick` takes nothing else. Kills: a tick that accepts stray arguments
    /// (a typo'd unit line would run something else silently).
    #[test]
    fn tick_takes_no_argument() {
        assert_eq!(parse_command(&a("tick")), Ok(Command::Tick));
        assert!(parse_command(&a("tick --apply")).is_err());
        assert_ne!(EXIT_BLOCKED, 0);
        assert_ne!(EXIT_BLOCKED, 1);
        assert_ne!(EXIT_BLOCKED, 2);
    }

    /// Kills: reading any other DSN variable, or tolerating one alongside.
    #[test]
    fn only_the_maintenance_dsn_is_read() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            resolve_url(env(&[(URL_VAR, "postgres://m/db")])).unwrap(),
            "postgres://m/db"
        );
        assert!(resolve_url(env(&[])).is_err());
        for v in FORBIDDEN_VARS {
            for value in ["y", ""] {
                let e = resolve_url(|k: &str| {
                    if k == URL_VAR {
                        Some("x".to_string())
                    } else if k == v {
                        Some(value.to_string())
                    } else {
                        None
                    }
                })
                .expect_err(v);
                assert!(e.contains(&format!("{v} is set")), "{e}");
            }
        }
    }
}
