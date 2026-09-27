//! Boot refusals of the two binaries (batch E1a: T-A4).
//!
//! Each case spawns the REAL binary with a CLEARED environment, a working
//! directory that has no `.env` in any ancestor (the REST binary calls
//! `dotenvy::dotenv()`, which searches upwards), and a `DATABASE_URL` that
//! points at a closed port on a `*_test` name. A refusal must happen BEFORE
//! the database connect: the assertion is on the refusal message AND on the
//! absence of the connect log line, so a restored development fallback (which
//! would get past the check and die later on the dead database) turns red.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEAD_DB: &str = "postgres://nobody:nothing@127.0.0.1:1/episcience_boot_refusal_test";
const CONNECT_LINE: &str = "Connecting to PostgreSQL";

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
    let scratch = tempfile::TempDir::new().expect("scratch dir");
    assert!(
        !scratch.path().ancestors().any(|d| d.join(".env").exists()),
        "scratch dir must have no .env in any ancestor"
    );
    let mut cmd = Command::new(bin);
    cmd.env_clear()
        .current_dir(scratch.path())
        .env("DATABASE_URL", DEAD_DB)
        .env("RUST_LOG", "info")
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
    let out = run(REST_BIN, &[("EPIGRAPH_JWT_SECRET", "boot-test-secret")]);
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
    let out = run(MCP_BIN, &[("EPIGRAPH_JWT_SECRET", "boot-test-secret")]);
    assert!(
        out.output.contains(CONNECT_LINE),
        "control must reach the database connect:\n{}",
        out.output
    );
}
