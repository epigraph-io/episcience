//! A minimal streamable-HTTP MCP client plus a server launcher, for tests that
//! must go through the REAL HTTP stack (`episcience_api::mcp::http::router`:
//! bearer layer -> rmcp session -> `ServerHandler::call_tool` -> tool).
//!
//! Included with `#[path = "support/mcp_http.rs"] mod mcp_http;`.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider};
use episcience_api::mcp::http::{router, McpHttpAuth};
use episcience_api::mcp::EpiscienceServer;
use episcience_api::middleware::JwtConfig;
use serde_json::{json, Value};
use sqlx::PgPool;

/// Serve the production MCP router on an ephemeral loopback port.
pub async fn start_mcp(pool: PgPool, blob_dir: PathBuf, auth: McpHttpAuth) -> SocketAddr {
    let embedder: Arc<dyn EmbeddingService> =
        Arc::new(MockProvider::new(EmbeddingConfig::openai(1536)));
    let server = EpiscienceServer::new(pool, embedder, blob_dir, 1024 * 1024);
    let app = router(server, auth);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    addr
}

pub fn bearer_auth(secret: &[u8]) -> McpHttpAuth {
    McpHttpAuth::Bearer(Arc::new(JwtConfig::from_secret(secret)))
}

pub struct McpClient {
    url: String,
    token: Option<String>,
    session: Option<String>,
    http: reqwest::Client,
    next_id: u64,
}

/// The outcome of one JSON-RPC request over HTTP.
pub struct Reply {
    pub status: reqwest::StatusCode,
    /// The JSON-RPC response object (with `result` or `error`), if any.
    pub body: Option<Value>,
}

impl Reply {
    pub fn result(&self) -> &Value {
        let body = self.body.as_ref().expect("a JSON-RPC body");
        assert!(body.get("error").is_none(), "unexpected error: {body}");
        &body["result"]
    }

    pub fn error_message(&self) -> String {
        let body = self.body.as_ref().expect("a JSON-RPC body");
        body["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("expected a JSON-RPC error, got {body}"))
            .to_string()
    }
}

impl McpClient {
    pub fn new(addr: SocketAddr, token: Option<String>) -> Self {
        Self {
            url: format!("http://{addr}/mcp"),
            token,
            session: None,
            http: reqwest::Client::new(),
            next_id: 1,
        }
    }

    async fn post(&mut self, message: Value, expect_reply: bool) -> Reply {
        let mut req = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(message.to_string());
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        let mut resp = req.send().await.expect("send MCP request");
        let status = resp.status();
        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            self.session = Some(sid.to_string());
        }
        if !status.is_success() || !expect_reply {
            return Reply { status, body: None };
        }
        // Read the SSE stream until the data line that answers this request.
        let want_id = message["id"].clone();
        let mut buf = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let chunk = tokio::time::timeout_at(deadline, resp.chunk())
                .await
                .expect("MCP reply within 20 s")
                .expect("read SSE chunk");
            let Some(chunk) = chunk else {
                panic!("SSE stream ended without a reply; got:\n{buf}");
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            for line in buf.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                        if v.get("id") == Some(&want_id) {
                            return Reply {
                                status,
                                body: Some(v),
                            };
                        }
                    }
                }
            }
        }
    }

    /// The session id the server assigned at `initialize`, if any.
    pub fn session(&self) -> Option<String> {
        self.session.clone()
    }

    /// A client that sends `token` on an EXISTING session (no initialize).
    pub fn on_session(addr: SocketAddr, token: Option<String>, session: String) -> Self {
        let mut c = Self::new(addr, token);
        c.session = Some(session);
        c
    }

    /// `DELETE /mcp` for the current session; returns the HTTP status.
    pub async fn delete_session(&mut self) -> reqwest::StatusCode {
        let mut req = self.http.delete(&self.url);
        if let Some(t) = &self.token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        req.send().await.expect("send DELETE").status()
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// `initialize` + `notifications/initialized`. Returns the HTTP status of
    /// the `initialize` POST (401 when the bearer layer refuses).
    pub async fn initialize(&mut self) -> reqwest::StatusCode {
        let id = self.id();
        let reply = self
            .post(
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "initialize",
                    "params": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {},
                        "clientInfo": {"name": "episcience-test", "version": "0"}
                    }
                }),
                true,
            )
            .await;
        if reply.status.is_success() {
            let note = self
                .post(
                    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                    false,
                )
                .await;
            assert!(note.status.is_success(), "initialized: {}", note.status);
        }
        reply.status
    }

    pub async fn list_tools(&mut self) -> Reply {
        let id = self.id();
        self.post(
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}}),
            true,
        )
        .await
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Reply {
        let id = self.id();
        self.post(
            json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }),
            true,
        )
        .await
    }
}

/// The JSON text a successful tool returns, parsed.
pub fn tool_json(result: &Value) -> Value {
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result has no text content: {result}"));
    serde_json::from_str(text).expect("tool text is JSON")
}
