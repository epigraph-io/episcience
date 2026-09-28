//! Boot of `episcience-worker` (T-J10) and of the server and the MCP binary
//! without the retired service client (T-J11), batch E1f.
//!
//! Each case spawns the REAL binary with a CLEARED environment from a scratch
//! directory with no `.env` in any ancestor, against a fresh clone of the E1
//! template on the test cluster.

#[path = "../../episcience-db/tests/support/mod.rs"]
mod support;

#[path = "support/token.rs"]
mod token;

#[path = "support/mcp_http.rs"]
mod mcp_http;

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use support::{TestDb, WORKER_LOGIN};

const WORKER_BIN: &str = env!("CARGO_BIN_EXE_episcience-worker");
const REST_BIN: &str = env!("CARGO_BIN_EXE_episcience-server");
const MCP_BIN: &str = env!("CARGO_BIN_EXE_episcience-mcp-server");
/// Passes the strength rule (>= 32 bytes, not the development literal).
const BOOT_SECRET: &str = "boot-test-secret-0123456789abcdef-e1f";
const CONNECT_LINE: &str = "Connecting to PostgreSQL";
const STARTED: &str = "episcience-worker started";
/// Any log line of the retired client's boot (`EpiGraph auth: …`).
const CLIENT_LINE: &str = "EpiGraph auth";

struct Proc {
    child: Child,
    output: Arc<Mutex<String>>,
    _scratch: tempfile::TempDir,
}

impl Proc {
    fn spawn(bin: &str, envs: &[(&str, String)]) -> Proc {
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn binary");
        let output = Arc::new(Mutex::new(String::new()));
        let pipes: Vec<Box<dyn Read + Send>> = vec![
            Box::new(child.stdout.take().unwrap()),
            Box::new(child.stderr.take().unwrap()),
        ];
        for mut pipe in pipes {
            let sink = Arc::clone(&output);
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    sink.lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }
        Proc {
            child,
            output,
            _scratch: scratch,
        }
    }

    /// Wait until the output contains `needle` (true) or the process exits
    /// (false). Panics after 60 s.
    fn wait_for(&mut self, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if self.output.lock().unwrap().contains(needle) {
                return true;
            }
            if self.child.try_wait().expect("try_wait").is_some() {
                std::thread::sleep(Duration::from_millis(200));
                return self.output.lock().unwrap().contains(needle);
            }
            assert!(Instant::now() < deadline, "no {needle:?} within 60 s");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the process to exit; its success flag.
    fn exit(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(s) = self.child.try_wait().expect("try_wait") {
                std::thread::sleep(Duration::from_millis(200));
                return s.success();
            }
            assert!(Instant::now() < deadline, "the process did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The output with every DSN in `secrets` masked (the clone's DSNs carry
    /// CI passwords; never echo them).
    fn text(&self, secrets: &[&str]) -> String {
        let mut t = self.output.lock().unwrap().clone();
        for s in secrets {
            t = t.replace(s, "<dsn>");
        }
        t
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// T-J10. The worker refuses to start, before any database I/O, when any of
/// the forbidden variables (`WORKER_FORBIDDEN_VARS`: privileged DSNs, the other
/// EpiScience logins' DSNs, the retired service client) is set (even empty), and
/// names it. Kills: a refusal removed, or moved after the connect.
#[tokio::test(flavor = "multi_thread")]
async fn t_j10_the_worker_refuses_every_forbidden_variable_before_connecting() {
    let db = TestDb::fresh().await;
    let url = db.login_url(WORKER_LOGIN);
    for var in episcience_api::config::WORKER_FORBIDDEN_VARS {
        let mut p = Proc::spawn(
            WORKER_BIN,
            &[
                (episcience_api::config::WORKER_DATABASE_URL_VAR, url.clone()),
                (var, String::new()),
            ],
        );
        assert!(!p.exit(), "{var}: the worker must exit non-zero");
        let out = p.text(&[&url]);
        assert!(out.contains(var), "{var}:\n{out}");
        assert!(
            !out.contains(CONNECT_LINE),
            "{var}: refused AFTER connecting:\n{out}"
        );
    }
}

/// T-J10, the session half: on a superuser DSN (the checkout's) the worker
/// connects and then refuses, naming the attribute. Kills: the privileged
/// session check removed (the worker would run bypassing row security).
#[tokio::test(flavor = "multi_thread")]
async fn t_j10_the_worker_refuses_a_superuser_session() {
    let db = TestDb::fresh().await;
    let url = db.url();
    let mut p = Proc::spawn(
        WORKER_BIN,
        &[(episcience_api::config::WORKER_DATABASE_URL_VAR, url.clone())],
    );
    assert!(!p.exit(), "the worker must refuse a superuser session");
    let out = p.text(&[&url]);
    assert!(out.contains("SUPERUSER"), "{out}");
    assert!(!out.contains(STARTED), "{out}");
}

/// Boot the worker on `url` and return `(exited non-zero, masked output)`.
/// A worker that STARTS fails the caller's assertion (it is killed on drop).
fn worker_refusal(url: &str) -> (bool, String) {
    let mut p = Proc::spawn(
        WORKER_BIN,
        &[(
            episcience_api::config::WORKER_DATABASE_URL_VAR,
            url.to_string(),
        )],
    );
    let started = p.wait_for(STARTED);
    if started {
        return (false, p.text(&[url]));
    }
    (!p.exit(), p.text(&[url]))
}

/// T-J10, the effective-role half (the reviewer's reproduction): a login that
/// is a member of a NOLOGIN BYPASSRLS role, with a DSN that switches onto
/// that role at connect time (`options=-c role=…`). The login's own row is
/// unprivileged, so a check of `session_user` alone passes it and every stage
/// transaction would bypass row security. The bypass role holds the
/// application grants, so no probe refuses it incidentally: only the
/// privileged-session check can. Kills: judging `session_user` only.
#[tokio::test(flavor = "multi_thread")]
async fn t_j10_the_worker_refuses_a_role_switch_onto_a_bypassrls_role() {
    let db = TestDb::fresh().await;
    let roles = db.privileged_roles().await;
    let url = db.url_as(&roles.login, &roles.password, Some(&roles.bypass));
    let (refused, out) = worker_refusal(&url);
    assert!(refused, "the worker must refuse:\n{out}");
    assert!(out.contains("role switch"), "{out}");
    assert!(out.contains("BYPASSRLS"), "{out}");
    assert!(!out.contains(STARTED), "{out}");
}

/// T-J10, the reachability arm: the same login WITHOUT a role switch. Its
/// session is not bypassing yet, but it can `SET ROLE` onto the BYPASSRLS
/// role at any time (membership, INHERIT or not), which is exactly the
/// misconfiguration the refusal exists for. Kills: dropping the membership
/// reachability check (keeping only the role-switch and `is_superuser`
/// arms).
#[tokio::test(flavor = "multi_thread")]
async fn t_j10_the_worker_refuses_a_login_that_can_reach_a_bypassrls_role() {
    let db = TestDb::fresh().await;
    let roles = db.privileged_roles().await;
    let url = db.url_as(&roles.login, &roles.password, None);
    let (refused, out) = worker_refusal(&url);
    assert!(refused, "the worker must refuse:\n{out}");
    assert!(
        out.contains("membership reaching a privileged role"),
        "{out}"
    );
    assert!(
        out.contains(&roles.bypass),
        "the refusal names the role:\n{out}"
    );
    assert!(!out.contains("a role switch ("), "no switch here:\n{out}");
}

/// T-J10, the seed arm (brief 7.6: no EpiScience login is ever a member of
/// the kernel seed role): a login holding the worker's application roles AND
/// `epigraph_seed`, with no privileged attribute and no role switch, is
/// refused before it starts, naming the seed role. Kills: dropping
/// `epigraph_seed` from the privileged-role set of
/// `refuse_privileged_session`.
#[tokio::test(flavor = "multi_thread")]
async fn t_j10_the_worker_refuses_a_login_that_is_a_member_of_the_seed_role() {
    let db = TestDb::fresh().await;
    let seed = db.seed_member_login().await;
    let (refused, out) = worker_refusal(&db.url_as(&seed.login, &seed.password, None));
    assert!(refused, "the worker must refuse:\n{out}");
    assert!(
        out.contains("membership reaching a privileged role") && out.contains("epigraph_seed"),
        "{out}"
    );
}

/// T-J10, the role-switch arm on its own: the real worker login switched onto
/// an UNPRIVILEGED role it is a member of (`episcience_rw`). Nothing
/// privileged is reachable, so only `session_user <> current_user` refuses
/// it (a switched process acts as a role the operator did not provision for
/// it). The assertion names the refusal, so a probe failing incidentally on
/// the narrower role cannot pass the test. Kills: dropping the role-switch
/// arm.
#[tokio::test(flavor = "multi_thread")]
async fn t_j10_the_worker_refuses_any_role_switch() {
    let db = TestDb::fresh().await;
    let url = db.url_as(WORKER_LOGIN.0, WORKER_LOGIN.1, Some("episcience_rw"));
    let (refused, out) = worker_refusal(&url);
    assert!(refused, "the worker must refuse:\n{out}");
    assert!(
        out.contains("role switch (the login episcience_worker runs as episcience_rw)"),
        "{out}"
    );
}

/// Control: on the worker login it passes every probe, starts, and its
/// sessions are attributable (`application_name=episcience-worker` on the
/// `episcience_worker` role). Kills: a refusal that rejects the healthy
/// deployment, and a dropped application name (the deploy smoke finds the
/// worker by it).
#[tokio::test(flavor = "multi_thread")]
async fn the_worker_boots_on_its_login_and_is_attributable() {
    let db = TestDb::fresh().await;
    let url = db.login_url(WORKER_LOGIN);
    let mut p = Proc::spawn(
        WORKER_BIN,
        &[(episcience_api::config::WORKER_DATABASE_URL_VAR, url.clone())],
    );
    assert!(p.wait_for(STARTED), "{}", p.text(&[&url]));
    assert!(p.text(&[&url]).contains("tenancy contract v1 probe OK"));
    let sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
          WHERE datname = current_database()
            AND usename = 'episcience_worker' AND application_name = 'episcience-worker'",
    )
    .fetch_one(&db.admin)
    .await
    .unwrap();
    assert!(sessions >= 1, "the worker's sessions are attributable");
}

/// T-J11. With `EPIGRAPH_CLIENT_ID` / `EPIGRAPH_CLIENT_SECRET` unset, the REST
/// server (in-process runner off) serves `/health`, and the MCP binary serves
/// `tools/list` over HTTP; neither logs the retired client's boot line. With
/// the variables SET, each warns that they are ignored and still serves.
/// Kills: a client still constructed at boot (it logged `EpiGraph auth: …`),
/// and a binary that needs the variables.
#[tokio::test(flavor = "multi_thread")]
async fn t_j11_server_and_mcp_serve_without_the_service_client() {
    let db = TestDb::fresh().await;
    // The request servers' own login (E1g): they refuse the superuser DSN.
    let url = db.login_url(support::APP_LOGIN);
    for with_vars in [false, true] {
        let mut extra: Vec<(&str, String)> = vec![
            ("DATABASE_URL", url.clone()),
            ("EPIGRAPH_JWT_SECRET", BOOT_SECRET.to_string()),
        ];
        if with_vars {
            extra.push(("EPIGRAPH_CLIENT_ID", "retired".into()));
            extra.push(("EPIGRAPH_CLIENT_SECRET", "retired".into()));
        }

        let port = free_port();
        let mut rest_env = extra.clone();
        rest_env.push(("EPISCIENCE_PORT", port.to_string()));
        rest_env.push(("EPISCIENCE_INPROCESS_WORKER", "0".into()));
        let mut rest = Proc::spawn(REST_BIN, &rest_env);
        assert!(rest.wait_for("listening on"), "{}", rest.text(&[&url]));
        let health = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .expect("GET /health");
        assert!(health.status().is_success());
        let out = rest.text(&[&url]);
        assert!(!out.contains(CLIENT_LINE), "{out}");
        assert!(!out.contains("reconciliation pass"), "runner off:\n{out}");
        assert_eq!(
            out.contains("EPIGRAPH_CLIENT_ID is set but ignored"),
            with_vars,
            "{out}"
        );

        let mport = free_port();
        let mut mcp_env = extra.clone();
        mcp_env.push(("EPISCIENCE_LISTEN", format!("127.0.0.1:{mport}")));
        let mut mcp = Proc::spawn(MCP_BIN, &mcp_env);
        assert!(mcp.wait_for("starting on"), "{}", mcp.text(&[&url]));
        let spec = token::TokenSpec {
            secret: BOOT_SECRET.as_bytes().to_vec(),
            ..token::TokenSpec::valid(uuid::Uuid::now_v7())
        };
        let addr: std::net::SocketAddr = format!("127.0.0.1:{mport}").parse().unwrap();
        // The listener binds just after the startup line.
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::net::TcpStream::connect(addr).is_err() {
            assert!(Instant::now() < deadline, "MCP listener never bound");
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut client = mcp_http::McpClient::new(addr, Some(token::mint(&spec)));
        assert_eq!(client.initialize().await, reqwest::StatusCode::OK);
        let tools = client.list_tools().await;
        assert!(
            tools.result()["tools"]
                .as_array()
                .is_some_and(|t| !t.is_empty()),
            "tools/list lists the tools"
        );
        let out = mcp.text(&[&url]);
        assert!(!out.contains(CLIENT_LINE), "{out}");
        assert_eq!(
            out.contains("EPIGRAPH_CLIENT_ID is set but ignored"),
            with_vars,
            "{out}"
        );
    }
}
