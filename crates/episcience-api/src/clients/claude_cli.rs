//! `claude -p` subprocess LLM provider — the OAuth (prepaid Max/Pro) path.
//!
//! Mirrors the epiclaw-host convention (`src/host/oauth.rs`,
//! `bridge-dev/orchestrate.py`): shell out to the Claude Code CLI with
//! `claude -p <prompt> --output-format json` rather than calling
//! `api.anthropic.com` directly. The CLI authenticates via the ambient
//! `~/.claude/.credentials.json` and **self-refreshes** the OAuth token on each
//! invocation (only `claude -p` rotates the token), so no `ANTHROPIC_API_KEY`,
//! no manual `CLAUDE_CODE_OAUTH_TOKEN` plumbing, and no 1-hour-expiry handling
//! are needed here.
//!
//! Implements the kernel `LlmProvider` trait, so it drops into the synthesis
//! pipeline exactly where `AnthropicClient` / `MockLlmClient` sit.
//!
//! The `--output-format json` envelope looks like:
//! ```json
//! {"type":"result","subtype":"success","is_error":false,"result":"<model text>", ...}
//! ```
//! `result` is the model's raw text (which, for our prompts, is the JSON we
//! asked for — possibly fenced in ```` ```json ````). We extract `result`,
//! strip any markdown fence, and parse it as the caller-facing JSON value.

use async_trait::async_trait;
use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
use std::time::Duration;
use tokio::process::Command;

/// Default wall-clock cap for a single `claude -p` call. Compose prompts carry
/// ~40 cluster summaries, so allow generous headroom; override with
/// `EPISCIENCE_CLAUDE_TIMEOUT_SECS`.
const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// LLM provider backed by the `claude` CLI in headless (`-p`) mode.
#[derive(Debug)]
pub struct ClaudeCliProvider {
    /// Binary to spawn (default `claude`; override `EPISCIENCE_CLAUDE_BIN`).
    binary: String,
    /// Optional `--model` selector; `None` uses the CLI's configured default.
    model: Option<String>,
    /// Per-call timeout.
    timeout: Duration,
}

impl ClaudeCliProvider {
    /// Build from the process environment:
    /// - `EPISCIENCE_CLAUDE_BIN` — binary path (default `claude`)
    /// - `EPISCIENCE_LLM_MODEL` — `--model` selector (optional)
    /// - `EPISCIENCE_CLAUDE_TIMEOUT_SECS` — per-call timeout (default 180)
    pub fn from_env() -> Self {
        Self {
            binary: std::env::var("EPISCIENCE_CLAUDE_BIN")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "claude".to_string()),
            model: std::env::var("EPISCIENCE_LLM_MODEL")
                .ok()
                .filter(|s| !s.is_empty()),
            timeout: Duration::from_secs(
                std::env::var("EPISCIENCE_CLAUDE_TIMEOUT_SECS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DEFAULT_TIMEOUT_SECS),
            ),
        }
    }

    /// Extract the caller-facing JSON from a `--output-format json` envelope.
    ///
    /// Kept pure (no process spawn) so the envelope contract is unit-testable.
    /// Scans for the last line that parses as a JSON object (the CLI prints the
    /// result object last; any diagnostic lines precede it), rejects error
    /// envelopes, then parses `result` — de-fencing markdown — as JSON.
    fn parse_envelope(stdout: &str) -> Result<serde_json::Value, LlmError> {
        let envelope: serde_json::Value = stdout
            .lines()
            .rev()
            .find_map(|line| {
                let line = line.trim();
                if line.starts_with('{') {
                    serde_json::from_str::<serde_json::Value>(line).ok()
                } else {
                    None
                }
            })
            .ok_or_else(|| LlmError::MalformedResponse {
                message: "claude -p produced no JSON envelope on stdout".to_string(),
            })?;

        let is_error = envelope
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let subtype = envelope
            .get("subtype")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if is_error || subtype != "success" {
            // A max-turns / usage-limit stop surfaces here; treat a rate/usage
            // limit as retryable so the pipeline's retry loop can back off.
            if subtype.contains("limit") || subtype.contains("rate") {
                return Err(LlmError::RateLimited {
                    retry_after_secs: 60,
                });
            }
            return Err(LlmError::RequestFailed {
                message: format!("claude -p returned non-success envelope (subtype={subtype:?})"),
            });
        }

        let result_text = envelope
            .get("result")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| LlmError::MalformedResponse {
                message: "claude -p envelope missing string `result`".to_string(),
            })?;

        let json_str = extract_json_from_text(result_text);
        serde_json::from_str(&json_str).map_err(|e| LlmError::MalformedResponse {
            message: format!("claude -p `result` is not JSON: {e}. Raw: {json_str}"),
        })
    }
}

#[async_trait]
impl LlmProvider for ClaudeCliProvider {
    fn name(&self) -> &str {
        "claude_cli"
    }

    fn model_name(&self) -> &str {
        self.model.as_deref().unwrap_or("claude-cli")
    }

    fn is_active(&self) -> bool {
        // Resolvable on PATH (or an absolute override that exists).
        which(&self.binary)
    }

    async fn complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError> {
        let mut cmd = Command::new(&self.binary);
        cmd.arg("-p").arg(prompt).arg("--output-format").arg("json");
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        // Never let the CLI block on an interactive stdin prompt.
        cmd.stdin(std::process::Stdio::null());

        let output = tokio::time::timeout(self.timeout, cmd.output())
            .await
            .map_err(|_| LlmError::RequestFailed {
                message: format!("claude -p timed out after {}s", self.timeout.as_secs()),
            })?
            .map_err(|e| LlmError::RequestFailed {
                message: format!("failed to spawn `{}`: {e}", self.binary),
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lower = stderr.to_lowercase();
            if lower.contains("rate limit")
                || lower.contains("429")
                || lower.contains("usage limit")
            {
                return Err(LlmError::RateLimited {
                    retry_after_secs: 60,
                });
            }
            return Err(LlmError::RequestFailed {
                message: format!("claude -p exited {}: {}", output.status, stderr.trim()),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        Self::parse_envelope(&stdout)
    }
}

/// Best-effort PATH lookup for `is_active`. An absolute/relative path that
/// exists counts; otherwise scan `PATH` entries.
fn which(binary: &str) -> bool {
    if binary.contains('/') {
        return std::path::Path::new(binary).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(binary).exists()))
        .unwrap_or(false)
}

/// Strip a markdown code fence (```` ```json ```` or bare ```` ``` ````) from
/// an LLM text response, returning the inner JSON. Mirrors the kernel
/// `AnthropicClient`'s private `extract_json_from_text` so behaviour is
/// consistent across providers.
fn extract_json_from_text(text: &str) -> String {
    let trimmed = text.trim();

    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        if let Some(end) = after.find("```") {
            let content = after[..end].trim();
            if content.starts_with('[') || content.starts_with('{') {
                return content.to_string();
            }
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_envelope_with_bare_json_result() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"{\"narrative\":\"hello\"}","session_id":"x"}"#;
        let v = ClaudeCliProvider::parse_envelope(stdout).expect("should parse");
        assert_eq!(v["narrative"], "hello");
    }

    #[test]
    fn parses_success_envelope_with_markdown_fenced_result() {
        // `result` text carries a ```json fence, as models often emit.
        let inner = "```json\n{\"narrative\":\"x\",\"n\":3}\n```";
        let envelope = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": inner,
        });
        let v = ClaudeCliProvider::parse_envelope(&envelope.to_string()).expect("should parse");
        assert_eq!(v["narrative"], "x");
        assert_eq!(v["n"], 3);
    }

    #[test]
    fn ignores_leading_diagnostic_lines_and_takes_last_json_object() {
        let stdout = "some warning line\nanother note\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"[1,2,3]\"}";
        let v = ClaudeCliProvider::parse_envelope(stdout).expect("should parse");
        assert_eq!(v, serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn error_envelope_is_request_failed() {
        let stdout =
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":""}"#;
        let err = ClaudeCliProvider::parse_envelope(stdout).unwrap_err();
        assert!(matches!(err, LlmError::RequestFailed { .. }));
    }

    #[test]
    fn usage_limit_envelope_is_rate_limited() {
        let stdout =
            r#"{"type":"result","subtype":"error_max_turns_limit","is_error":true,"result":""}"#;
        let err = ClaudeCliProvider::parse_envelope(stdout).unwrap_err();
        assert!(matches!(err, LlmError::RateLimited { .. }));
    }

    #[test]
    fn missing_result_field_is_malformed() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false}"#;
        let err = ClaudeCliProvider::parse_envelope(stdout).unwrap_err();
        assert!(matches!(err, LlmError::MalformedResponse { .. }));
    }

    #[test]
    fn no_json_object_on_stdout_is_malformed() {
        let err = ClaudeCliProvider::parse_envelope("not json at all\n").unwrap_err();
        assert!(matches!(err, LlmError::MalformedResponse { .. }));
    }

    #[test]
    fn model_name_reflects_override() {
        std::env::set_var("EPISCIENCE_LLM_MODEL", "claude-opus-4-8");
        let p = ClaudeCliProvider::from_env();
        assert_eq!(p.model_name(), "claude-opus-4-8");
        std::env::remove_var("EPISCIENCE_LLM_MODEL");
    }
}
