//! Boot refusals of the two binaries (batch E1a: T-A4).
//!
//! Each case spawns the REAL binary with a CLEARED environment, a working
//! directory that has no `.env` in any ancestor (the REST binary calls
//! `dotenvy::dotenv()`, which searches upwards), and a `DATABASE_URL` that
//! points at a closed port on a `*_test` name. A refusal must happen BEFORE
//! the database connect: the assertion is on the refusal message AND on the
//! absence of the connect log line, so a restored development fallback (which
//! would get past the check and die later on the dead database) turns red.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEAD_DB: &str = "postgres://nobody:nothing@127.0.0.1:1/episcience_boot_refusal_test";
const CONNECT_LINE: &str = "Connecting to PostgreSQL";
/// A secret that passes the strength rule (>= 32 bytes, not the dev literal).
const BOOT_SECRET: &str = "boot-test-secret-0123456789abcdef-e1a";
/// The kernel's committed development literal.
const DEV_LITERAL: &str = "epigraph-dev-secret-change-in-production!!";

struct Outcome {
    /// `None` when the process was stopped by the harness after it logged
    /// the database connect line (it would otherwise retry for a while).
    success: Option<bool>,
    output: String,
}

/// Run `bin` with exactly `envs`, from a scratch directory. Returns when the
/// process exits, or kills it as soon as it logs [`CONNECT_LINE`] (proof it
/// passed every boot refusal). Fails the test if neither happens in 45 s.
fn run(bin: &str, envs: &[(&str, &str)]) -> Outcome {
    let os: Vec<(&str, &OsStr)> = envs.iter().map(|(k, v)| (*k, OsStr::new(*v))).collect();
    run_with(bin, &[], true, &os)
}

/// [`run`] with arguments, values that need not be UTF-8, and `DATABASE_URL` set to the
/// dead database only when `dead_database_url` (the worker, maint and migrate
/// binaries refuse that variable outright).
fn run_with(bin: &str, args: &[&str], dead_database_url: bool, envs: &[(&str, &OsStr)]) -> Outcome {
    let scratch = tempfile::TempDir::new().expect("scratch dir");
    assert!(
        !scratch.path().ancestors().any(|d| d.join(".env").exists()),
        "scratch dir must have no .env in any ancestor"
    );
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .env_clear()
        .current_dir(scratch.path())
        .env("RUST_LOG", "info")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if dead_database_url {
        cmd.env("DATABASE_URL", DEAD_DB);
    }
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
    let deadline = Instant::now() + Duration::from_secs(45);
    let success = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status.success());
        }
        if output.lock().unwrap().contains(CONNECT_LINE) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{bin} neither exited nor reached the database within 45 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    for r in readers {
        let _ = r.join();
    }
    let output = output.lock().unwrap().clone();
    Outcome { success, output }
}

const REST_BIN: &str = env!("CARGO_BIN_EXE_episcience-server");
const MCP_BIN: &str = env!("CARGO_BIN_EXE_episcience-mcp-server");
const WORKER_BIN: &str = env!("CARGO_BIN_EXE_episcience-worker");
const MAINT_BIN: &str = env!("CARGO_BIN_EXE_episcience-maint");
const MIGRATE_BIN: &str = env!("CARGO_BIN_EXE_episcience-migrate");

// T-A4 (REST). Kills: restoring the `DEV_JWT_SECRET` fallback, or moving the
// check after the database connect.
#[test]
fn rest_server_refuses_to_boot_without_the_secret() {
    let out = run(REST_BIN, &[]);
    assert_eq!(
        out.success,
        Some(false),
        "must exit non-zero:\n{}",
        out.output
    );
    assert!(
        out.output.contains("EPIGRAPH_JWT_SECRET must be set"),
        "must name the missing secret:\n{}",
        out.output
    );
    assert!(
        !out.output.contains(CONNECT_LINE),
        "must refuse before touching the database:\n{}",
        out.output
    );

    // Empty counts as unset.
    let out = run(REST_BIN, &[("EPIGRAPH_JWT_SECRET", "")]);
    assert!(out.output.contains("EPIGRAPH_JWT_SECRET must be set"));

    // Control: with the secret the process gets PAST the check and reaches
    // the (dead) database.
    let out = run(REST_BIN, &[("EPIGRAPH_JWT_SECRET", BOOT_SECRET)]);
    assert!(
        !out.output.contains("EPIGRAPH_JWT_SECRET must be set"),
        "control must pass the secret check:\n{}",
        out.output
    );
    assert!(
        out.output.contains(CONNECT_LINE),
        "control must reach the database connect:\n{}",
        out.output
    );
}

// T-A4 (MCP), both transports. Kills: dropping the secret requirement from
// the MCP boot gate.
#[test]
fn mcp_server_refuses_to_boot_without_the_secret() {
    for envs in [vec![], vec![("EPISCIENCE_LISTEN", "127.0.0.1:0")]] {
        let out = run(MCP_BIN, &envs);
        assert_eq!(
            out.success,
            Some(false),
            "{envs:?}: must exit non-zero:\n{}",
            out.output
        );
        assert!(
            out.output.contains("EPIGRAPH_JWT_SECRET must be set"),
            "{envs:?}: must name the missing secret:\n{}",
            out.output
        );
        assert!(
            !out.output.contains(CONNECT_LINE),
            "{envs:?}: must refuse before touching the database:\n{}",
            out.output
        );
    }

    // Control: with the secret the process reaches the database connect.
    let out = run(MCP_BIN, &[("EPIGRAPH_JWT_SECRET", BOOT_SECRET)]);
    assert!(
        out.output.contains(CONNECT_LINE),
        "control must reach the database connect:\n{}",
        out.output
    );
}

// T-B0. Kills: removing the wildcard-bind refusal, checking it after the
// database connect, or judging the address without canonicalising it first
// (`::ffff:0.0.0.0` is an IPv6 socket that listens on every IPv4 interface).
#[test]
fn rest_server_refuses_a_wildcard_bind() {
    for wildcard in ["0.0.0.0", "::", "::ffff:0.0.0.0", "0:0:0:0:0:ffff:0:0"] {
        let out = run(
            REST_BIN,
            &[
                ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
                ("EPISCIENCE_BIND_ADDR", wildcard),
            ],
        );
        assert_eq!(
            out.success,
            Some(false),
            "{wildcard}: must exit non-zero:\n{}",
            out.output
        );
        assert!(
            out.output.contains("EPISCIENCE_BIND_ADDR") && out.output.contains("refused"),
            "{wildcard}: must name the refused bind:\n{}",
            out.output
        );
        assert!(
            !out.output.contains(CONNECT_LINE),
            "{wildcard}: must refuse before touching the database:\n{}",
            out.output
        );
    }

    // Controls: the default (loopback), an IPv4-mapped loopback and a specific
    // non-loopback address all pass the bind check and reach the database
    // connect.
    for envs in [
        vec![("EPIGRAPH_JWT_SECRET", BOOT_SECRET)],
        vec![
            ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
            ("EPISCIENCE_BIND_ADDR", "::ffff:127.0.0.1"),
        ],
        vec![
            ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
            ("EPISCIENCE_BIND_ADDR", "192.0.2.10"),
        ],
    ] {
        let out = run(REST_BIN, &envs);
        assert!(
            out.output.contains(CONNECT_LINE),
            "{envs:?}: control must reach the database connect:\n{}",
            out.output
        );
    }
}

// The MCP HTTP listener gets the same wildcard rule, and the development
// opt-out is refused on a non-loopback address. Kills: removing
// `mcp_listen_guard` from the MCP boot, checking it after the database
// connect, or judging the address without canonicalising it.
#[test]
fn mcp_server_refuses_a_wildcard_or_exposed_unauthenticated_listener() {
    let refused: [(&str, Vec<(&str, &str)>); 4] = [
        (
            "wildcard v4",
            vec![
                ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
                ("EPISCIENCE_LISTEN", "0.0.0.0:0"),
            ],
        ),
        (
            "wildcard v6",
            vec![
                ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
                ("EPISCIENCE_LISTEN", "[::]:0"),
            ],
        ),
        (
            "wildcard v4-mapped",
            vec![
                ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
                ("EPISCIENCE_LISTEN", "[::ffff:0.0.0.0]:0"),
            ],
        ),
        (
            "unauthenticated on a non-loopback address",
            vec![
                ("EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP", "1"),
                ("EPISCIENCE_LISTEN", "192.0.2.10:0"),
            ],
        ),
    ];
    for (label, envs) in refused {
        let out = run(MCP_BIN, &envs);
        assert_eq!(
            out.success,
            Some(false),
            "{label}: must exit non-zero:\n{}",
            out.output
        );
        assert!(
            out.output.contains("EPISCIENCE_LISTEN") && out.output.contains("refused"),
            "{label}: must name the refused listener:\n{}",
            out.output
        );
        assert!(
            !out.output.contains(CONNECT_LINE),
            "{label}: must refuse before touching the database:\n{}",
            out.output
        );
    }

    // Controls: loopback with a token, and the opt-out on loopback, reach the
    // database connect.
    for envs in [
        vec![
            ("EPIGRAPH_JWT_SECRET", BOOT_SECRET),
            ("EPISCIENCE_LISTEN", "127.0.0.1:0"),
        ],
        vec![
            ("EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP", "1"),
            ("EPISCIENCE_LISTEN", "127.0.0.1:0"),
        ],
    ] {
        let out = run(MCP_BIN, &envs);
        assert!(
            out.output.contains(CONNECT_LINE),
            "{envs:?}: control must reach the database connect:\n{}",
            out.output
        );
    }
}

// Both binaries refuse a secret shorter than 32 bytes and the committed
// development literal, before the database connect (the kernel's
// `assert_production_secret` rule). Kills: dropping the length check, the
// literal check, or the MCP binary's call to the strength rule.
#[test]
fn both_binaries_refuse_a_weak_or_development_secret() {
    let short = "s".repeat(31);
    for bin in [REST_BIN, MCP_BIN] {
        for (label, secret, needle) in [
            ("31 bytes", short.as_str(), "minimum is 32"),
            ("dev literal", DEV_LITERAL, "committed dev literal"),
        ] {
            let out = run(bin, &[("EPIGRAPH_JWT_SECRET", secret)]);
            assert_eq!(
                out.success,
                Some(false),
                "{bin} {label}: must exit non-zero:\n{}",
                out.output
            );
            assert!(
                out.output.contains("EPIGRAPH_JWT_SECRET is refused")
                    && out.output.contains(needle),
                "{bin} {label}: must name the refused secret:\n{}",
                out.output
            );
            assert!(
                !out.output.contains(CONNECT_LINE),
                "{bin} {label}: must refuse before touching the database:\n{}",
                out.output
            );
        }
        // Control: exactly 32 bytes passes and reaches the connect.
        let out = run(bin, &[("EPIGRAPH_JWT_SECRET", &"k".repeat(32))]);
        assert!(
            out.output.contains(CONNECT_LINE),
            "{bin} 32 bytes: control must reach the database connect:\n{}",
            out.output
        );
    }
}

/// A variable set to a value that is NOT UTF-8 is SET: every presence refusal
/// refuses it and the in-process switch rejects it, before any database I/O.
/// `std::env::var(..).ok()` reads such a value as unset, so each of these
/// binaries used to boot past its refusal (the worker with a retired client
/// variable or a superuser `DATABASE_URL` beside its own DSN). Control: the
/// worker with only its own DSN gets past every refusal to the connect.
/// Kills: reading presence through `std::env::var(..).ok()` in the worker,
/// `episcience-maint`, `episcience-migrate` or the server's switch.
#[test]
fn a_non_utf8_value_is_refused_never_read_as_unset() {
    let bad = OsString::from_vec(vec![b'x', 0xff]);
    let dead = OsStr::new(DEAD_DB);
    let worker_dsn = episcience_api::config::WORKER_DATABASE_URL_VAR;

    let control = run_with(WORKER_BIN, &[], false, &[(worker_dsn, dead)]);
    assert!(
        control.output.contains(CONNECT_LINE),
        "control: the worker passes its refusals:\n{}",
        control.output
    );

    for var in ["EPIGRAPH_CLIENT_ID", "DATABASE_URL"] {
        let out = run_with(WORKER_BIN, &[], false, &[(worker_dsn, dead), (var, &bad)]);
        assert_eq!(out.success, Some(false), "worker, {var}:\n{}", out.output);
        assert!(
            out.output.contains(&format!("{var} is set")),
            "worker, {var}:\n{}",
            out.output
        );
        assert!(!out.output.contains(CONNECT_LINE), "{}", out.output);
    }

    let out = run_with(
        MAINT_BIN,
        &["tick"],
        false,
        &[
            ("EPISCIENCE_MAINT_DATABASE_URL", dead),
            ("DATABASE_URL", &bad),
        ],
    );
    assert_eq!(out.success, Some(false), "maint:\n{}", out.output);
    assert!(
        out.output.contains("DATABASE_URL is set"),
        "maint:\n{}",
        out.output
    );

    let out = run_with(
        MIGRATE_BIN,
        &["status"],
        false,
        &[
            ("EPISCIENCE_MIGRATION_DATABASE_URL", dead),
            ("DATABASE_URL", &bad),
        ],
    );
    assert_eq!(out.success, Some(false), "migrate:\n{}", out.output);
    assert!(
        out.output.contains("DATABASE_URL is set"),
        "migrate:\n{}",
        out.output
    );

    let out = run_with(
        REST_BIN,
        &[],
        true,
        &[
            ("EPIGRAPH_JWT_SECRET", OsStr::new(BOOT_SECRET)),
            ("EPISCIENCE_INPROCESS_WORKER", &bad),
        ],
    );
    assert_eq!(out.success, Some(false), "server:\n{}", out.output);
    assert!(
        out.output.contains("EPISCIENCE_INPROCESS_WORKER="),
        "server:\n{}",
        out.output
    );
    assert!(!out.output.contains(CONNECT_LINE), "{}", out.output);
}
