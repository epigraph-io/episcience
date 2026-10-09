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
//! If that strict parse fails, raw control characters inside strings and
//! trailing commas are repaired (see `repair_json`) and the text is parsed
//! once more; anything else stays `MalformedResponse`.

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
        // Strict parse first: text that is valid as emitted is never touched,
        // so a well-formed response parses exactly as it always has.
        let strict_err = match serde_json::from_str(&json_str) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        // The model sometimes emits near-JSON: raw newlines inside a string
        // value, or a trailing comma. Repair only those two defects and parse
        // again; anything else stays malformed. On failure, report the
        // ORIGINAL error and the text as emitted, not the repaired text.
        let repaired = repair_json(&json_str);
        match serde_json::from_str(&repaired) {
            Ok(v) => {
                tracing::warn!(
                    error = %strict_err,
                    "claude -p `result` was not strict JSON; parsed after repairing raw control characters / trailing commas"
                );
                Ok(v)
            }
            Err(_) => Err(LlmError::MalformedResponse {
                message: format!("claude -p `result` is not JSON: {strict_err}. Raw: {json_str}"),
            }),
        }
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
        // Pure text completion: expose NO tools so the agentic CLI cannot take
        // filesystem/bash side effects in the service's working directory (it
        // was observed writing a stray `*.md` during compose). Without tools the
        // model can only return the JSON we asked for.
        cmd.arg("--tools").arg("");
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

/// Repair the two near-JSON defects models have been observed to emit, and
/// nothing else:
///
/// - a raw control character (U+0000..U+001F) INSIDE a string literal is
///   replaced by its JSON escape (`\n`, `\r`, `\t`, or `\u00XX`);
/// - a comma OUTSIDE every string literal whose next non-whitespace
///   character is `}` or `]` (a trailing comma) is dropped.
///
/// String boundaries are tracked with backslash escapes honoured, so an
/// escaped quote never ends a string, and in-string text (including `,}`)
/// is copied unchanged. Text outside strings, including whitespace, is
/// copied unchanged. Works on `char`s, so multi-byte UTF-8 passes through
/// intact. Never invents values, quotes or brackets: the caller re-parses
/// the result with `serde_json`, which still rejects anything else.
fn repair_json(text: &str) -> String {
    let is_json_ws = |c: char| matches!(c, ' ' | '\t' | '\n' | '\r');
    let mut out = String::with_capacity(text.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
            } else if c == '\\' {
                escaped = true;
                out.push(c);
            } else if c == '"' {
                in_string = false;
                out.push(c);
            } else if (c as u32) < 0x20 {
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push_str(&format!("\\u{:04x}", c as u32)),
                }
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            ',' => {
                // `,` is one byte, so `i + 1` is a char boundary.
                let rest = text[i + 1..].trim_start_matches(is_json_ws);
                if !(rest.starts_with('}') || rest.starts_with(']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
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

    // ── Tolerant parse of the model's `result` text ──────────────────────
    //
    // The CLI envelope is always valid JSON (the CLI writes it); the model's
    // `result` text inside it is not always. These tests build a realistic
    // envelope whose `result` decodes to the exact text the model emitted.

    /// Wrap `result_text` in a success envelope as `claude -p
    /// --output-format json` prints it (one line, `result` JSON-escaped).
    fn success_envelope(result_text: &str) -> String {
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 9123,
            "num_turns": 1,
            "result": result_text,
            "session_id": "00000000-0000-4000-8000-000000000000",
            "total_cost_usd": 0.0,
        })
        .to_string()
    }

    fn malformed_message(err: LlmError) -> String {
        match err {
            LlmError::MalformedResponse { message } => message,
            other => panic!("expected MalformedResponse, got {other:?}"),
        }
    }

    /// (1) A pretty-printed object whose `summary` string spans lines: the
    /// model put RAW newlines inside the string value. serde_json rejects
    /// that ("control character (\u0000-\u001F) found while parsing a string
    /// at line 4 column 0"); the repair escapes them and the value keeps
    /// the newlines.
    #[test]
    fn repairs_raw_newlines_inside_a_string_value() {
        let result = "{\n  \"title\": \"Thermal stability of the lattice\",\n  \"summary\": \"Staple density sets the melting window [0190a3c4-1111-7000-8000-000000000001].\n\nA second paragraph follows the blank line.\"\n}";
        assert!(
            serde_json::from_str::<serde_json::Value>(result)
                .unwrap_err()
                .to_string()
                .contains("control character"),
            "fixture must reproduce the strict-parse failure"
        );
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("raw newlines inside a string must be repaired");
        assert_eq!(v["title"], "Thermal stability of the lattice");
        assert_eq!(
            v["summary"],
            "Staple density sets the melting window [0190a3c4-1111-7000-8000-000000000001].\n\nA second paragraph follows the blank line."
        );
    }

    /// (1b) Raw tab and carriage return inside a string, beside non-ASCII
    /// scientific text: the repair escapes every U+0000..U+001F control
    /// character and must not corrupt multi-byte UTF-8.
    #[test]
    fn repairs_raw_control_chars_without_corrupting_non_ascii() {
        let result = "{\"summary\": \"Held at 37 °C in 12 mM Mg²⁺,\r\n\tlength 2 µm — stable.\"}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("raw control characters inside a string must be repaired");
        assert_eq!(
            v["summary"],
            "Held at 37 °C in 12 mM Mg²⁺,\r\n\tlength 2 µm — stable."
        );
    }

    /// (2) A trailing comma before the closing brace on a long single-line
    /// object (serde_json: "trailing comma at line 1 column N").
    #[test]
    fn repairs_trailing_comma_before_closing_brace() {
        let long = "x".repeat(600);
        let result = format!("{{\"title\":\"T\",\"summary\":\"{long}\",}}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&result)
                .unwrap_err()
                .to_string()
                .contains("trailing comma"),
            "fixture must reproduce the strict-parse failure"
        );
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(&result))
            .expect("a trailing comma before `}` must be repaired");
        assert_eq!(v["title"], "T");
        assert_eq!(v["summary"].as_str().unwrap().len(), 600);
    }

    /// (2b) A trailing comma before a closing bracket, with whitespace
    /// between the comma and the bracket.
    #[test]
    fn repairs_trailing_comma_before_closing_bracket() {
        let result = "{\"ids\": [1, 2, 3 ,\n ], \"n\": 3}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("a trailing comma before `]` must be repaired");
        assert_eq!(v, serde_json::json!({"ids": [1, 2, 3], "n": 3}));
    }

    /// (3) JSON in a ```json fence is still de-fenced (guard), and a fenced
    /// body that also needs repair is repaired after de-fencing.
    #[test]
    fn fenced_json_is_defenced_and_repaired() {
        let plain = "```json\n{\"title\": \"A\", \"summary\": \"B\"}\n```";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(plain))
            .expect("fenced valid JSON must parse");
        assert_eq!(v, serde_json::json!({"title": "A", "summary": "B"}));

        let broken =
            "```json\n{\n  \"title\": \"A\",\n  \"summary\": \"line one\nline two\",\n}\n```";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(broken))
            .expect("fenced JSON with a raw newline and a trailing comma must be repaired");
        assert_eq!(
            v,
            serde_json::json!({"title": "A", "summary": "line one\nline two"})
        );
    }

    /// (4) Already-valid JSON parses exactly as serde_json parses it — the
    /// repair never runs on text that is valid as emitted, even when that
    /// text holds `,}` / `\n` sequences inside strings.
    #[test]
    fn valid_json_parses_identically_to_strict_serde() {
        let result = "{\n  \"title\": \"Edge, case\",\n  \"summary\": \"escaped \\\"quote\\\", escaped \\\\n, and ,} inside\",\n  \"n\": [1, 2.5, -3e2, null, true]\n}";
        let strict: serde_json::Value = serde_json::from_str(result).expect("fixture is valid");
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result)).expect("valid JSON");
        assert_eq!(v, strict);
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            serde_json::to_string(&strict).unwrap()
        );
    }

    /// (5) Prose is not JSON and the repair must not make it so: the error
    /// stays MalformedResponse and carries the raw excerpt.
    #[test]
    fn prose_stays_malformed_with_raw_excerpt() {
        let result =
            "I'm unable to summarize these claims,\nbut here is what I can say: {not json,}";
        let msg = malformed_message(
            ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
        );
        assert!(msg.contains("is not JSON"), "message: {msg}");
        assert!(
            msg.contains("I'm unable to summarize these claims"),
            "raw excerpt missing from message: {msg}"
        );
    }

    /// (6) Raw newlines OUTSIDE strings are valid JSON whitespace: a
    /// pretty-printed object that needs a trailing-comma repair keeps its
    /// structure (escaping those newlines would make the text invalid).
    #[test]
    fn raw_newlines_outside_strings_are_left_alone() {
        let result = "{\n\t\"title\": \"A\",\r\n  \"tags\": [\n    \"x\",\n    \"y\",\n  ],\n}\n";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("pretty JSON with trailing commas must be repaired");
        assert_eq!(v, serde_json::json!({"title": "A", "tags": ["x", "y"]}));
    }

    /// (7) Escapes inside strings: an escaped quote must not end the string,
    /// an escaped `\\n` must stay a literal backslash-n, and an escaped
    /// backslash right before the closing quote must still close it — all
    /// next to a raw newline that does need escaping.
    #[test]
    fn repair_respects_escapes_inside_strings() {
        let result = concat!(
            r#"{"summary": "He said \"hi\" then"#,
            "\n",
            r#"a literal \\n and a backslash \\", "n": 1}"#
        );
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("raw newline beside escapes must be repaired");
        assert_eq!(
            v["summary"],
            "He said \"hi\" then\na literal \\n and a backslash \\"
        );
        assert_eq!(v["n"], 1);
    }

    /// (8) A comma followed by `}` or `]` INSIDE a string is content, not a
    /// trailing comma; a real trailing comma elsewhere in the same object
    /// forces the repair path so the in-string text goes through it.
    #[test]
    fn repair_keeps_commas_inside_strings() {
        let result = r#"{"summary": "set {a,}", "tail": "b, ]", "list": [1,2,],}"#;
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("trailing commas outside strings must be repaired");
        assert_eq!(v["summary"], "set {a,}");
        assert_eq!(v["tail"], "b, ]");
        assert_eq!(v["list"], serde_json::json!([1, 2]));
    }

    /// When the repair cannot rescue the text, the error reports the
    /// ORIGINAL strict-parse failure and the text the model emitted, not
    /// the repaired text.
    #[test]
    fn unrepairable_json_reports_the_original_error_and_raw_text() {
        // Raw newline in a string (repairable) plus a missing value
        // (not repairable): the repaired text still fails.
        let result = "{\"summary\": \"a\nb\", \"n\": }";
        let msg = malformed_message(
            ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
        );
        assert!(
            msg.contains("control character"),
            "expected the original strict-parse error, got: {msg}"
        );
        assert!(
            msg.contains("\"a\nb\""),
            "expected the unrepaired raw text, got: {msg}"
        );
    }

    /// (6b) The repair is the identity on text with no defect: whitespace
    /// outside strings (raw newlines, CR, tabs), escapes and non-ASCII text
    /// come back byte-for-byte.
    #[test]
    fn repair_is_identity_on_valid_json() {
        let text = "{\r\n\t\"a\": \"x \\\"y\\\" \\\\ \\n µm\",\n  \"b\": [1, {\"c\": \",}\"}]\n}\n";
        serde_json::from_str::<serde_json::Value>(text).expect("fixture is valid");
        assert_eq!(repair_json(text), text);
    }

    #[test]
    fn model_name_reflects_override() {
        std::env::set_var("EPISCIENCE_LLM_MODEL", "claude-opus-4-8");
        let p = ClaudeCliProvider::from_env();
        assert_eq!(p.model_name(), "claude-opus-4-8");
        std::env::remove_var("EPISCIENCE_LLM_MODEL");
    }
}
