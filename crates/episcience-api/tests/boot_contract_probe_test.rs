//! T-C1: both binaries run the tenancy contract probe at boot, after the
//! database connect and before serving, and refuse a database whose kernel
//! lost a contract item that fails silently at run time (C11: the application
//! role's INSERT on `events`, which the in-process event publisher would
//! otherwise swallow).
//!
//! Each case spawns the REAL binary with a cleared environment against a
//! throwaway clone of the test template, as the `episcience_app` application
//! login (the only DSN the binaries serve on from E1g).
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;
use testdb::{TestDb, APP_LOGIN};

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REST_BIN: &str = env!("CARGO_BIN_EXE_episcience-server");
const MCP_BIN: &str = env!("CARGO_BIN_EXE_episcience-mcp-server");
/// A secret that passes the strength rule (>= 32 bytes, not the dev literal).
const BOOT_SECRET: &str = "boot-test-secret-0123456789abcdef-e1c";
const PROBE_OK: &str = "tenancy contract v1 probe OK";
/// Logged by the REST server only after the probe (and every other boot
/// refusal): it is about to serve.
const RECONCILE_LINE: &str = "EpiScience ELN server listening on";
/// Logged by the MCP binary only after the probe (its embedder selection).
/// Exit status alone cannot show the MCP refusal: a stdio server whose stdin
/// closes exits non-zero on its own.
const MCP_AFTER_PROBE: &str = "for synthesis embeddings";

struct Outcome {
    /// `None` when the harness stopped the process after it logged `stop_at`.
    success: Option<bool>,
    output: String,
}

/// Run `bin` with exactly `envs` from a scratch directory (no `.env` in any
/// ancestor). Returns when the process exits, or kills it as soon as its
/// output contains `stop_at`. Fails the test if neither happens in 60 s.
fn run(bin: &str, envs: &[(String, String)], stop_at: &str) -> Outcome {
    let scratch = tempfile::TempDir::new().expect("scratch dir");
    assert!(
        !scratch.path().ancestors().any(|d| d.join(".env").exists()),
        "scratch dir must have no .env in any ancestor"
    );
    let mut cmd = Command::new(bin);
    cmd.env_clear()
        .current_dir(scratch.path())
        .env("RUST_LOG", "info")
        .env("EPISCIENCE_BLOB_DIR", scratch.path().join("blobs"))
        .env("EPIGRAPH_API_URL", "http://127.0.0.1:1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn binary");
    let output = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    let pipes: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for mut pipe in pipes {
        let sink = Arc::clone(&output);
        readers.push(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let success = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status.success());
        }
        if output.lock().unwrap().contains(stop_at) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{bin} neither exited nor logged {stop_at:?} within 60 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    for r in readers {
        let _ = r.join();
    }
    let output = output.lock().unwrap().clone();
    // The clone's DSN carries the CI superuser password; never echo it.
    let output = output.replace(&db_url_marker(envs), "<database url>");
    Outcome { success, output }
}

fn db_url_marker(envs: &[(String, String)]) -> String {
    envs.iter()
        .find(|(k, _)| k == "DATABASE_URL")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "\u{0}".into())
}

fn envs(db: &TestDb, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v = vec![
        ("DATABASE_URL".to_string(), db.login_url(APP_LOGIN)),
        ("EPIGRAPH_JWT_SECRET".to_string(), BOOT_SECRET.to_string()),
    ];
    v.extend(extra.iter().map(|(k, x)| (k.to_string(), x.to_string())));
    v
}

async fn blocking(
    bin: &'static str,
    envs: Vec<(String, String)>,
    stop_at: &'static str,
) -> Outcome {
    tokio::task::spawn_blocking(move || run(bin, &envs, stop_at))
        .await
        .expect("join")
}

/// Kills: removing the probe call from either binary, logging instead of
/// exiting on failure, or running it after the first write path.
#[tokio::test(flavor = "multi_thread")]
async fn both_binaries_refuse_a_database_missing_c11() {
    let db = TestDb::fresh().await;
    sqlx::query("REVOKE INSERT ON public.events FROM epigraph_app")
        .execute(&db.admin)
        .await
        .expect("revoke");
    let still: bool =
        sqlx::query_scalar("SELECT has_table_privilege('epigraph_app', 'public.events', 'INSERT')")
            .fetch_one(&db.admin)
            .await
            .expect("check");
    assert!(!still, "the revoke must have taken effect");

    let rest = blocking(
        REST_BIN,
        envs(&db, &[("EPISCIENCE_PORT", "0")]),
        RECONCILE_LINE,
    )
    .await;
    assert_eq!(
        rest.success,
        Some(false),
        "REST must exit non-zero:\n{}",
        rest.output
    );
    assert!(
        rest.output.contains("contract v1 probe failed") && rest.output.contains("C11"),
        "REST must name C11:\n{}",
        rest.output
    );
    assert!(
        !rest.output.contains(RECONCILE_LINE),
        "REST must refuse before its first write path:\n{}",
        rest.output
    );

    let mcp = blocking(MCP_BIN, envs(&db, &[]), MCP_AFTER_PROBE).await;
    assert_eq!(
        mcp.success,
        Some(false),
        "MCP must exit non-zero:\n{}",
        mcp.output
    );
    assert!(
        mcp.output.contains("contract v1 probe failed") && mcp.output.contains("C11"),
        "MCP must name C11:\n{}",
        mcp.output
    );
    assert!(
        !mcp.output.contains(MCP_AFTER_PROBE),
        "MCP must stop at the refusal, before building anything:\n{}",
        mcp.output
    );
}

/// Control: on an intact clone both binaries pass the probe and carry on.
/// Kills: a probe that refuses a healthy database (every deploy would stop).
#[tokio::test(flavor = "multi_thread")]
async fn both_binaries_pass_the_probe_on_an_intact_database() {
    let db = TestDb::fresh().await;
    let rest = blocking(
        REST_BIN,
        envs(&db, &[("EPISCIENCE_PORT", "0")]),
        RECONCILE_LINE,
    )
    .await;
    assert!(rest.output.contains(PROBE_OK), "REST:\n{}", rest.output);
    assert!(
        rest.output.contains(RECONCILE_LINE),
        "REST must carry on past the probe:\n{}",
        rest.output
    );
    let mcp = blocking(MCP_BIN, envs(&db, &[]), MCP_AFTER_PROBE).await;
    assert!(mcp.output.contains(PROBE_OK), "MCP:\n{}", mcp.output);
    assert!(
        mcp.output.contains(MCP_AFTER_PROBE),
        "MCP must carry on past the probe:\n{}",
        mcp.output
    );
    assert!(!mcp.output.contains("probe failed"), "MCP:\n{}", mcp.output);
}

/// A kernel-only clone migrated to EXACTLY `version` (the ledger's own
/// `run_to`).
async fn at_version(version: i64) -> TestDb {
    let db = TestDb::fresh_kernel_only().await;
    let mut c = episcience_db::ledger::connect_with(db.admin_options())
        .await
        .expect("ledger connection");
    episcience_db::ledger::run_to(&mut c, Some(version))
        .await
        .expect("episcience migrations");
    db
}

/// E1d review R16: both binaries refuse a database whose EpiScience schema
/// predates the tenancy columns (5033: the E1c schema) and name the missing
/// columns; on 5034 ALONE (the deploy installs this binary between 5034 and
/// 5035) both pass and carry on. Kills: the schema probe removed from either
/// binary, or one that demands 5035 (the deploy order would break).
#[tokio::test(flavor = "multi_thread")]
async fn both_binaries_refuse_a_schema_without_the_tenancy_columns_and_accept_5034() {
    let old = at_version(5033).await;
    let rest = blocking(
        REST_BIN,
        envs(&old, &[("EPISCIENCE_PORT", "0")]),
        RECONCILE_LINE,
    )
    .await;
    assert_eq!(rest.success, Some(false), "REST:\n{}", rest.output);
    assert!(
        rest.output.contains("schema probe failed")
            && rest.output.contains("syntheses.owner_group_id"),
        "REST must name the missing columns:\n{}",
        rest.output
    );
    let mcp = blocking(MCP_BIN, envs(&old, &[]), MCP_AFTER_PROBE).await;
    assert_eq!(mcp.success, Some(false), "MCP:\n{}", mcp.output);
    assert!(
        mcp.output.contains("schema probe failed") && !mcp.output.contains(MCP_AFTER_PROBE),
        "MCP must stop at the refusal:\n{}",
        mcp.output
    );

    let window = at_version(episcience_db::ledger::TENANCY_EXPAND_VERSION).await;
    let rest = blocking(
        REST_BIN,
        envs(&window, &[("EPISCIENCE_PORT", "0")]),
        RECONCILE_LINE,
    )
    .await;
    assert!(
        rest.output.contains("schema probe OK") && rest.output.contains(RECONCILE_LINE),
        "REST must run on 5034 alone:\n{}",
        rest.output
    );
    let mcp = blocking(MCP_BIN, envs(&window, &[]), MCP_AFTER_PROBE).await;
    assert!(
        mcp.output.contains("schema probe OK") && mcp.output.contains(MCP_AFTER_PROBE),
        "MCP must run on 5034 alone:\n{}",
        mcp.output
    );
}

/// T-E1 (T18): both binaries refuse to serve on a SUPERUSER DSN (the
/// checkout's former runtime) and refuse to start with
/// `MAINTENANCE_DATABASE_URL` set, each before serving and naming why; the
/// application login is served (the control above). Kills: the privileged
/// session refusal dropped from `EpiscienceDb::connect`, or the
/// privileged-variable refusal dropped from either binary.
#[tokio::test(flavor = "multi_thread")]
async fn t_e1_both_binaries_refuse_a_superuser_dsn_and_a_maintenance_dsn_variable() {
    let db = TestDb::fresh().await;
    let superuser = vec![
        ("DATABASE_URL".to_string(), db.url()),
        ("EPIGRAPH_JWT_SECRET".to_string(), BOOT_SECRET.to_string()),
    ];
    let mut rest_env = superuser.clone();
    rest_env.push(("EPISCIENCE_PORT".to_string(), "0".to_string()));
    let rest = blocking(REST_BIN, rest_env, RECONCILE_LINE).await;
    assert_eq!(rest.success, Some(false), "REST:\n{}", rest.output);
    assert!(
        rest.output.contains("privileged or switched") && rest.output.contains("SUPERUSER"),
        "REST must name the privileged session:\n{}",
        rest.output
    );
    assert!(!rest.output.contains(RECONCILE_LINE), "{}", rest.output);
    let mcp = blocking(MCP_BIN, superuser, MCP_AFTER_PROBE).await;
    assert_eq!(mcp.success, Some(false), "MCP:\n{}", mcp.output);
    assert!(
        mcp.output.contains("privileged or switched") && !mcp.output.contains(MCP_AFTER_PROBE),
        "MCP must refuse before building anything:\n{}",
        mcp.output
    );

    for bin in [REST_BIN, MCP_BIN] {
        let mut e = envs(
            &db,
            &[
                (
                    "MAINTENANCE_DATABASE_URL",
                    "postgres://x@127.0.0.1:1/x_test",
                ),
                ("EPISCIENCE_PORT", "0"),
            ],
        );
        e.dedup();
        let stop = if bin == REST_BIN {
            RECONCILE_LINE
        } else {
            MCP_AFTER_PROBE
        };
        let out = blocking(bin, e, stop).await;
        assert_eq!(out.success, Some(false), "{bin}:\n{}", out.output);
        assert!(
            out.output.contains("MAINTENANCE_DATABASE_URL is set"),
            "{bin} must name the variable:\n{}",
            out.output
        );
    }
}
