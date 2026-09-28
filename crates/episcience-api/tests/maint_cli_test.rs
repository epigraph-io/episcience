//! `episcience-maint backfill-owners`: the REAL binary against a kernel-only
//! clone migrated to 5034 (the deploy's window before the contract step),
//! through the real `episcience_maint` login.
//!
//! Each case spawns the binary with a CLEARED environment (only the variables
//! the case names) from a scratch directory.
#[path = "../../episcience-db/tests/support/mod.rs"]
mod testdb;

use std::process::Command;

use episcience_db::ledger;
use testdb::{TestDb, MAINT_LOGIN};
use uuid::Uuid;

const BIN: &str = env!("CARGO_BIN_EXE_episcience-maint");

async fn at_5034() -> TestDb {
    let db = TestDb::fresh_kernel_only().await;
    let mut c = ledger::connect_with(db.admin_options())
        .await
        .expect("ledger connection");
    ledger::run_to(&mut c, Some(ledger::TENANCY_EXPAND_VERSION))
        .await
        .expect("episcience migrations up to 5034");
    db
}

/// One legacy protocol by a shared agent (no personal group), no pair.
async fn legacy_protocol(db: &TestDb) -> Uuid {
    let shared = Uuid::new_v4();
    let mut pk = [0u8; 32];
    pk[..16].copy_from_slice(shared.as_bytes());
    sqlx::query(
        "INSERT INTO agents (id, public_key, display_name, agent_type, role, state) \
         VALUES ($1, $2, $3, 'service', 'custom', 'active')",
    )
    .bind(shared)
    .bind(&pk[..])
    .bind(format!("shared-{shared}"))
    .execute(&db.admin)
    .await
    .unwrap();
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO protocols (id, title, authored_by, content_hash) \
         VALUES ($1, 'legacy', $2, decode(repeat('01', 32), 'hex'))",
    )
    .bind(id)
    .bind(shared)
    .execute(&db.admin)
    .await
    .unwrap();
    id
}

fn run(args: &[&str], envs: &[(&str, String)], dir: &std::path::Path) -> (bool, String) {
    let mut cmd = Command::new(BIN);
    cmd.env_clear().current_dir(dir).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run episcience-maint");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

async fn owner_of(db: &TestDb, id: Uuid) -> Option<Uuid> {
    sqlx::query_scalar("SELECT owner_group_id FROM protocols WHERE id = $1")
        .bind(id)
        .fetch_one(&db.admin)
        .await
        .unwrap()
}

/// Dry run, a refused apply (wrong --expect-group), the apply, and the
/// reverse, end to end. Kills: a dry run that applies, an --expect-group
/// check skipped or run after the apply, a manifest that is not the reverse's
/// input, a manifest written world-readable or over an existing file.
#[tokio::test]
async fn backfill_owners_dry_run_apply_and_reverse() {
    let db = at_5034().await;
    let p = testdb::principal(&db.admin, "operator").await;
    let legacy = legacy_protocol(&db).await;
    let dir = tempfile::TempDir::new().unwrap();
    let env = vec![("EPISCIENCE_MAINT_DATABASE_URL", db.login_url(MAINT_LOGIN))];
    let principal = p.agent.to_string();
    let dry = dir.path().join("dry.json");

    let (ok, out) = run(
        &[
            "backfill-owners",
            "--principal",
            &principal,
            "--dry-run",
            "--manifest",
            dry.to_str().unwrap(),
        ],
        &env,
        dir.path(),
    );
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!("target_group={}", p.personal_group)),
        "{out}"
    );
    assert_eq!(
        owner_of(&db, legacy).await,
        None,
        "a dry run changes nothing"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dry).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the manifest is private");
    }
    let (ok, out) = run(
        &[
            "backfill-owners",
            "--principal",
            &principal,
            "--dry-run",
            "--manifest",
            dry.to_str().unwrap(),
        ],
        &env,
        dir.path(),
    );
    assert!(!ok && out.contains("never overwritten"), "{out}");

    let applied = dir.path().join("apply.json");
    let wrong = Uuid::new_v4().to_string();
    let (ok, out) = run(
        &[
            "backfill-owners",
            "--principal",
            &principal,
            "--apply",
            "--manifest",
            applied.to_str().unwrap(),
            "--expect-group",
            &wrong,
        ],
        &env,
        dir.path(),
    );
    assert!(!ok && out.contains("nothing changed"), "{out}");
    assert_eq!(owner_of(&db, legacy).await, None);
    assert!(!applied.exists());

    let group = p.personal_group.to_string();
    let (ok, out) = run(
        &[
            "backfill-owners",
            "--principal",
            &principal,
            "--apply",
            "--manifest",
            applied.to_str().unwrap(),
            "--expect-group",
            &group,
        ],
        &env,
        dir.path(),
    );
    assert!(ok, "{out}");
    assert_eq!(owner_of(&db, legacy).await, Some(p.personal_group));

    let (ok, out) = run(
        &["backfill-owners", "--reverse", applied.to_str().unwrap()],
        &env,
        dir.path(),
    );
    assert!(ok && out.contains("restored 1 rows"), "{out}");
    assert_eq!(owner_of(&db, legacy).await, None);
}

/// The binary reads only its own DSN and refuses a broader session. Kills:
/// falling back to DATABASE_URL, tolerating a migration, kernel maintenance
/// or worker DSN alongside (every `MAINT_FORBIDDEN_VARS` entry, the binary
/// itself), or running the definers on a superuser session.
#[tokio::test]
async fn the_binary_refuses_other_dsns_and_privileged_sessions() {
    let db = at_5034().await;
    let p = testdb::principal(&db.admin, "operator").await;
    let dir = tempfile::TempDir::new().unwrap();
    let m = dir.path().join("m.json");
    let args = [
        "backfill-owners",
        "--principal",
        &p.agent.to_string(),
        "--dry-run",
        "--manifest",
        m.to_str().unwrap(),
    ]
    .map(str::to_string);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let maint = db.login_url(MAINT_LOGIN);
    for extra in episcience_api::config::MAINT_FORBIDDEN_VARS {
        let (ok, out) = run(
            &args,
            &[
                ("EPISCIENCE_MAINT_DATABASE_URL", maint.clone()),
                (extra, maint.clone()),
            ],
            dir.path(),
        );
        assert!(
            !ok && out.contains(&format!("{extra} is set")),
            "{extra}: {out}"
        );
    }
    let (ok, out) = run(&args, &[("DATABASE_URL", maint.clone())], dir.path());
    assert!(!ok && out.contains("DATABASE_URL is set"), "{out}");
    let (ok, out) = run(
        &args,
        &[("EPISCIENCE_MAINT_DATABASE_URL", db.url())],
        dir.path(),
    );
    assert!(
        !ok && out.contains("SUPERUSER"),
        "a superuser DSN is refused, naming the attribute: {out}"
    );
    assert!(!m.exists(), "no refused run writes a manifest");
}
