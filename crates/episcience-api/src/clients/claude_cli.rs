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
//! The whole `result` is tried first and the fenced body only after it, so a
//! fence inside a JSON string never replaces the response. If a strict parse
//! fails, raw control characters inside strings and trailing commas are
//! repaired (see `repair_json`) and the text is parsed once more, accepting
//! only an object or array; anything else stays `MalformedResponse`. Prose
//! followed by unfenced JSON parses from the first `{` / `[` to the last
//! `}` / `]`.
//!
//! A safety-classifier stop still arrives as `subtype=success`: the CLI
//! answers with a fallback model. It is detected by a second model in
//! `modelUsage` (before any parse), or by prose that says it was stopped:
//! the prose around an extracted JSON span, or the whole text when no JSON
//! parses. A result that is JSON as a whole, and the content of an extracted
//! JSON span, are never read for that wording. A stop is reported as
//! `RequestFailed` with [`CLASSIFIER_STOP_PREFIX`], so it is named and not
//! retried as if it were a parse error.
//!
//! The child is isolated in code, not by deploy configuration: no built-in
//! tools, no user-level settings, no MCP servers (see `ISOLATION_ARGS`), and a
//! private, empty, per-call working directory.

use async_trait::async_trait;
use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};
use std::time::Duration;
use tokio::process::Command;

/// Default wall-clock cap for a single `claude -p` call. Compose prompts carry
/// ~40 cluster summaries, so allow generous headroom; override with
/// `EPISCIENCE_CLAUDE_TIMEOUT_SECS`.
const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// Flags that cut the `claude -p` child off from the invoking user's Claude
/// Code configuration, so the worker's model never holds the operator's
/// authority:
/// - `--setting-sources project,local` skips user-level settings (plugins and
///   their hooks, the advisor, connector config); the per-call working
///   directory is empty, so there are no project/local settings to load.
/// - `--strict-mcp-config` with no `--mcp-config` loads zero MCP servers,
///   claude.ai connectors included.
const ISOLATION_ARGS: &[&str] = &["--setting-sources", "project,local", "--strict-mcp-config"];

/// Stable prefix of the [`LlmError::RequestFailed`] message for a call a
/// safety classifier stopped. Callers may match on it (the kernel `LlmError`
/// has no refusal variant yet). A stop depends on the prompt's content, so
/// it is reported as a request failure, which the synthesis pipeline does
/// not retry, instead of a retryable `MalformedResponse`.
pub const CLASSIFIER_STOP_PREFIX: &str = "claude -p stopped by safety classifier";

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

    /// The `claude` argv for one completion.
    ///
    /// Kept pure so the isolation contract is unit-testable. `--tools ""`
    /// removes the built-in tools only; it does NOT remove MCP tools or the
    /// operator's settings (plugins, hooks, the advisor, claude.ai
    /// connectors), all of which the CLI would otherwise inherit from the OS
    /// user the worker runs as. [`ISOLATION_ARGS`] closes that: it is appended
    /// last and unconditionally, so no configuration (the `--model` override
    /// included) can drop it.
    fn build_args(prompt: &str, model: Option<&str>) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "json".into(),
            "--tools".into(),
            String::new(),
        ];
        if let Some(model) = model {
            args.push("--model".into());
            args.push(model.into());
        }
        args.extend(ISOLATION_ARGS.iter().map(|a| (*a).to_string()));
        args
    }

    /// Extract the caller-facing JSON from a `--output-format json` envelope.
    ///
    /// Kept pure (no process spawn) so the envelope contract is unit-testable.
    /// Scans for the last line that parses as a JSON object (the CLI prints the
    /// result object last; any diagnostic lines precede it), rejects error
    /// envelopes, a multi-model `modelUsage` (a classifier stop, reported as
    /// `RequestFailed` with [`CLASSIFIER_STOP_PREFIX`]) and an empty
    /// `result`, then parses `result` — whole, then de-fenced, then the
    /// bracketed span — as JSON. Prose outside an extracted span (or the
    /// whole text, when nothing parses) that reports a safety-classifier stop
    /// is also a classifier stop.
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
            // A max-turns / usage-limit stop surfaces here; report a rate/usage
            // limit as `RateLimited` so a caller can back off. (The synthesis
            // pipeline has no backoff and treats it as terminal for the prompt.)
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

        // Signal 1 is decided BEFORE any JSON is parsed or extracted, so a
        // fallback model's answer can never be accepted as JSON.
        //
        // Signal 1: `modelUsage` names more than one model. The child runs
        // with no advisor and no tools (`ISOLATION_ARGS`), so a second model
        // is the CLI's fallback after the requested model was stopped, even
        // when the fallback's text parses. Keys are resolved model ids
        // (`claude-opus-5-5[1m]`), not the `--model` string (which may be an
        // alias), so the check counts distinct models rather than comparing
        // against `--model`.
        if let Some(models) = fallback_models(&envelope) {
            // Logged so a misfire (a second model for some reason other than
            // a stop) shows up in the worker log, not only as failed rows.
            tracing::warn!(
                models = %models.join(", "),
                "claude -p modelUsage reports more than one model; treating the call as a safety-classifier stop"
            );
            return Err(classifier_stop(&format!(
                "modelUsage reports more than one model ({})",
                models.join(", ")
            )));
        }

        let trimmed = result_text.trim();
        if trimmed.is_empty() {
            return Err(LlmError::RequestFailed {
                message: "claude -p returned an empty result".to_string(),
            });
        }

        // Candidate 1: the whole `result`. It goes first so that a fence or
        // a brace INSIDE a JSON string (a Markdown narrative with a code
        // sample) can never replace the response with that fragment. A JSON
        // answer as a whole is an answer, whatever its content says.
        let mut last_strict_err = match parse_candidate(trimmed) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };

        // Then JSON extracted from around prose: the body of a markdown
        // fence, then the span from the first `{` (or `[`) to the last `}`
        // (or `]`), the earlier opener first. Each goes through the same
        // strict-then-repair parse; a candidate equal to one already tried
        // is skipped. Every candidate is a subslice of `trimmed`.
        let mut tried: Vec<&str> = vec![trimmed];
        for candidate in
            std::iter::once(extract_json_from_text(trimmed)).chain(bracket_spans(trimmed))
        {
            if tried.contains(&candidate) {
                continue;
            }
            tried.push(candidate);
            match parse_candidate(candidate) {
                Ok(v) => {
                    // Signal 2, for extracted JSON: the prose AROUND the
                    // parsed span (before and after it) says a safety
                    // classifier stopped the answer. The JSON's own content
                    // is not looked at, so an answer that discusses a safety
                    // filter is still an answer, while a fresh answer behind
                    // a stop notice is never laundered into a success.
                    let start = candidate.as_ptr() as usize - trimmed.as_ptr() as usize;
                    let end = start + candidate.len();
                    if mentions_safety_classifier(&trimmed[..start])
                        || mentions_safety_classifier(&trimmed[end..])
                    {
                        return Err(classifier_stop(
                            "the result text reports a safety classifier stop",
                        ));
                    }
                    return Ok(v);
                }
                Err(e) => last_strict_err = e,
            }
        }

        // Signal 2, when nothing parsed: the whole text (prose only) says a
        // safety classifier stopped it.
        if mentions_safety_classifier(trimmed) {
            return Err(classifier_stop(
                "the result text reports a safety classifier stop",
            ));
        }
        // Nothing parsed: report the ORIGINAL strict-parse error (of the last
        // candidate tried) and the text as emitted, not a repaired text.
        Err(LlmError::MalformedResponse {
            message: format!("claude -p `result` is not JSON: {last_strict_err}. Raw: {trimmed}"),
        })
    }
}

/// Parse one candidate text: strictly first (a candidate that is valid as
/// emitted is never touched, so a well-formed response parses as it always
/// has), then once more after [`repair_json`]. The model sometimes emits
/// near-JSON: raw control characters inside a string value, or a trailing
/// comma. Only an object or array is accepted from the repair: our prompts
/// ask for one, and a bare repaired string would turn quoted multi-line
/// prose into "JSON". On failure, returns the strict-parse error.
fn parse_candidate(candidate: &str) -> Result<serde_json::Value, serde_json::Error> {
    let strict_err = match serde_json::from_str(candidate) {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    if let Ok(v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) =
        serde_json::from_str(&repair_json(candidate))
    {
        tracing::warn!(
            error = %strict_err,
            "claude -p `result` was not strict JSON; parsed after repairing raw control characters / trailing commas"
        );
        return Ok(v);
    }
    Err(strict_err)
}

/// The distinct models in the envelope's `modelUsage` when there is more
/// than one (sorted), else `None`. A trailing context suffix such as `[1m]`
/// is ignored, so one model reported under its resolved id counts once.
fn fallback_models(envelope: &serde_json::Value) -> Option<Vec<String>> {
    let usage = envelope.get("modelUsage")?.as_object()?;
    let mut models: Vec<String> = usage
        .keys()
        .map(|k| k.split('[').next().unwrap_or(k).trim().to_string())
        .collect();
    models.sort();
    models.dedup();
    (models.len() > 1).then_some(models)
}

/// Whether prose (never JSON content) says a safety classifier stopped the
/// answer, in the wordings the CLI's fallback has been recorded using.
fn mentions_safety_classifier(text: &str) -> bool {
    let lower = text.to_lowercase();
    ["safety classifier", "safety filter"]
        .iter()
        .any(|p| lower.contains(p))
}

/// The classifier-stop error: [`CLASSIFIER_STOP_PREFIX`] plus which signal
/// fired. Never carries the model's text.
fn classifier_stop(signal: &str) -> LlmError {
    LlmError::RequestFailed {
        message: format!("{CLASSIFIER_STOP_PREFIX}: {signal}"),
    }
}

/// The spans from the first `{` to the last `}` and from the first `[` to
/// the last `]` of `text`, the one whose opener comes first first. Lets
/// prose followed by UNFENCED JSON parse.
fn bracket_spans(text: &str) -> Vec<&str> {
    let mut spans: Vec<(usize, &str)> = [('{', '}'), ('[', ']')]
        .iter()
        .filter_map(|&(open, close)| {
            let start = text.find(open)?;
            let end = text.rfind(close)?;
            (end > start).then(|| (start, &text[start..=end]))
        })
        .collect();
    spans.sort_by_key(|(start, _)| *start);
    spans.into_iter().map(|(_, span)| span).collect()
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
        // A private (0700), empty, per-call working directory: the CLI never
        // runs in the service's own directory (no project settings or
        // CLAUDE.md picked up from it, no stray files written into it) nor in
        // a world-writable one. Bound to a local so it outlives the child;
        // removed when this call returns.
        let mut builder = tempfile::Builder::new();
        builder.prefix("episcience-claude-");
        // tempfile's default for a directory is 0o777 minus the umask
        // (0o775 under a 002 umask): ask for owner-only explicitly.
        #[cfg(unix)]
        builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let workdir = builder.tempdir().map_err(|e| LlmError::RequestFailed {
            message: format!("failed to create a private working directory for claude -p: {e}"),
        })?;
        // Resolved before the child's cwd moves to `workdir` (see `spawn_path`).
        let mut cmd = Command::new(spawn_path(&self.binary));
        cmd.args(Self::build_args(prompt, self.model.as_deref()));
        cmd.current_dir(workdir.path());
        // Never let the CLI block on an interactive stdin prompt.
        cmd.stdin(std::process::Stdio::null());
        // On timeout the `output()` future is dropped: kill the child with it,
        // so it neither keeps spending quota nor outlives `workdir`.
        cmd.kill_on_drop(true);

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

/// The program to spawn for `binary`. A bare name stays bare (a `PATH`
/// lookup, as in [`which`]). A relative path with a directory part
/// (`bin/claude`) is made absolute against the current working directory:
/// that is where [`which`] found it, while the child runs from its per-call
/// directory, against which the OS would otherwise resolve it.
fn spawn_path(binary: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(binary);
    if binary.contains('/') && path.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            return cwd.join(path);
        }
    }
    path.to_path_buf()
}

/// Strip a markdown code fence (```` ```json ```` or bare ```` ``` ````) from
/// an LLM text response, returning the inner JSON. Mirrors the kernel
/// `AnthropicClient`'s private `extract_json_from_text` so behaviour is
/// consistent across providers.
fn extract_json_from_text(text: &str) -> &str {
    let trimmed = text.trim();

    if let Some(start) = trimmed.find("```json") {
        let after = &trimmed[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        if let Some(end) = after.find("```") {
            let content = after[..end].trim();
            if content.starts_with('[') || content.starts_with('{') {
                return content;
            }
        }
    }
    trimmed
}

/// Repair the two near-JSON defects models have been observed to emit, and
/// nothing else:
///
/// - a raw control character (U+0000..U+001F) INSIDE a string literal is
///   replaced by its JSON escape (`\n`, `\r`, `\t`, or `\u00XX`);
/// - a comma OUTSIDE every string literal that follows a value and whose
///   next non-whitespace character is `}` or `]` (a trailing comma) is
///   dropped. A comma right after `{`, `[`, `,` or `:` follows no value and
///   is kept, so `{,}` or `[1,,]` stay invalid instead of becoming `{}`.
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
    // Last non-whitespace char outside strings (a closed string counts as
    // `"`); `None` before the first one.
    let mut last_sig: Option<char> = None;
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
                last_sig = Some('"');
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
                let closes = rest.starts_with('}') || rest.starts_with(']');
                let after_value = !matches!(last_sig, None | Some('{' | '[' | ',' | ':'));
                if !(closes && after_value) {
                    out.push(c);
                    last_sig = Some(c);
                }
            }
            _ => {
                if !is_json_ws(c) {
                    last_sig = Some(c);
                }
                out.push(c);
            }
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

    /// (4) Already-valid JSON yields exactly the value strict serde_json
    /// yields, even when it holds `,}` / `\n` sequences inside strings.
    /// (Whether the repair also ran is not observable here: on valid JSON it
    /// is the identity, see `repair_is_identity_on_valid_json`.)
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

    // ── Escape tracking: fixtures whose outcome depends on it ────────────
    //
    // A tracker that ignored escapes (or mishandled `\\` before a closing
    // quote) would desync its notion of "inside a string" in each of these,
    // and either reject a repairable response or silently alter a value.

    /// An ODD number of escaped quotes before the raw newline: ignoring the
    /// escape would leave the newline "outside" the string, unrepaired.
    #[test]
    fn repair_tracks_an_odd_escaped_quote_before_a_raw_newline() {
        let result = concat!(r#"{"summary": "a 5\" disk"#, "\n", r#"next line", "n": 1}"#);
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("raw newline after an escaped quote must be repaired");
        assert_eq!(v["summary"], "a 5\" disk\nnext line");
        assert_eq!(v["n"], 1);
    }

    /// An escaped quote followed by an in-string `,}`, plus real trailing
    /// commas later: the in-string comma is content and must survive.
    #[test]
    fn repair_keeps_an_in_string_comma_after_an_escaped_quote() {
        let result = r#"{"summary": "say \" ok ,}", "tags": ["x",],}"#;
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("trailing commas outside strings must be repaired");
        assert_eq!(v["summary"], "say \" ok ,}");
        assert_eq!(v["tags"], serde_json::json!(["x"]));
    }

    /// A PAIR of escaped quotes around an in-string `,}`, plus a raw newline
    /// that forces the repair path. Ignoring escapes here does not fail: it
    /// drops the in-string comma and yields valid JSON with an altered value.
    #[test]
    fn repair_does_not_alter_a_value_between_paired_escaped_quotes() {
        let result = concat!(
            r#"{"summary": "He said \"stop ,}\" twice"#,
            "\n",
            r#"then left"}"#
        );
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("raw newline beside escaped quotes must be repaired");
        assert_eq!(v["summary"], "He said \"stop ,}\" twice\nthen left");
    }

    /// An escaped backslash right before a closing quote closes the string;
    /// the raw newline in the NEXT string must still be repaired.
    #[test]
    fn repair_closes_a_string_ending_in_an_escaped_backslash() {
        let result = concat!(r#"{"a": "x\\", "b": "line1"#, "\n", r#"line2"}"#);
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("raw newline after a `\\\\`-closed string must be repaired");
        assert_eq!(v["a"], "x\\");
        assert_eq!(v["b"], "line1\nline2");
    }

    /// Control characters other than `\n` / `\r` / `\t` (here U+000B and
    /// U+0000) are escaped as `\u00XX` and round-trip.
    #[test]
    fn repairs_other_control_chars_as_unicode_escapes() {
        let result = "{\"summary\": \"col1\u{b}col2\u{0}end\nnext\"}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("U+000B / U+0000 inside a string must be repaired");
        assert_eq!(v["summary"], "col1\u{b}col2\u{0}end\nnext");
    }

    // ── The whole `result` is tried before de-fencing ────────────────────
    //
    // A Markdown narrative can itself contain a code fence. De-fencing first
    // would replace the whole response with that fragment.

    /// Valid JSON whose string value contains a ```json fence is returned
    /// whole, not as the fenced fragment.
    #[test]
    fn valid_json_with_an_inner_fence_is_returned_whole() {
        let result = r#"{"narrative": "Intro\n```json\n[1, 2]\n```\nend"}"#;
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("valid JSON must parse");
        assert_eq!(
            v,
            serde_json::json!({"narrative": "Intro\n```json\n[1, 2]\n```\nend"})
        );
    }

    /// Raw newlines in a narrative that contains a fenced near-JSON snippet:
    /// the repair rescues the narrative; the snippet is not returned.
    #[test]
    fn raw_newline_narrative_with_an_inner_fence_keeps_the_narrative() {
        let result = "{\"narrative\": \"Intro\n```json\n[1, 2,]\n```\nend\"}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("the narrative must be repaired, not replaced by the snippet");
        assert_eq!(
            v,
            serde_json::json!({"narrative": "Intro\n```json\n[1, 2,]\n```\nend"})
        );
    }

    /// Same, with an escaped-quote snippet that is not JSON on its own.
    #[test]
    fn raw_newline_narrative_with_an_escaped_inner_fence_is_repaired() {
        let result = concat!(
            r#"{"narrative": "Intro"#,
            "\n```json\n",
            r#"{\"k\": 1}"#,
            "\n```\nend\"}"
        );
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("the narrative must be repaired");
        assert_eq!(v["narrative"], "Intro\n```json\n{\"k\": 1}\n```\nend");
    }

    /// Prose followed by a fenced JSON answer is still rescued from the fence.
    #[test]
    fn prose_then_fenced_json_is_still_rescued() {
        let result =
            "I'll summarize the cluster.\n```json\n{\"title\": \"A\", \"summary\": \"B\"}\n```";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("fenced JSON after prose must parse");
        assert_eq!(v, serde_json::json!({"title": "A", "summary": "B"}));
    }

    // ── The repair never manufactures a value from no value ──────────────

    /// A comma with no value before it is not a trailing comma: `{,}`,
    /// `[,]`, `{"a":,}` and `[1,,]` stay malformed.
    #[test]
    fn a_comma_without_a_preceding_value_is_not_dropped() {
        for result in ["{,}", "[,]", "{\"a\":,}", "[1,,]", "{\"a\": [ , ]}"] {
            let err =
                ClaudeCliProvider::parse_envelope(&success_envelope(result)).expect_err(result);
            assert!(
                matches!(err, LlmError::MalformedResponse { .. }),
                "{result}: {err:?}"
            );
        }
        // A trailing comma after every value kind is still dropped.
        let result = r#"{"s": "x", "n": 1, "t": true, "z": null, "o": {"a": 1,}, "l": [2,],}"#;
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("trailing commas after values must be repaired");
        assert_eq!(
            v,
            serde_json::json!({"s": "x", "n": 1, "t": true, "z": null, "o": {"a": 1}, "l": [2]})
        );
    }

    /// Quoted prose with raw newlines would repair into a bare JSON string;
    /// the repair path accepts only an object or array, so it stays
    /// malformed. A strictly valid scalar still parses as before.
    #[test]
    fn quoted_multiline_prose_stays_malformed() {
        let result = "\"I can't summarize these claims because\nthey are unrelated.\"";
        let msg = malformed_message(
            ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
        );
        assert!(msg.contains("they are unrelated."), "message: {msg}");

        let v = ClaudeCliProvider::parse_envelope(&success_envelope("\"strict string\""))
            .expect("a strictly valid scalar is untouched by the repair path");
        assert_eq!(v, serde_json::json!("strict string"));
    }

    // ── Classifier stops are named, not reported as a parse error ────────
    //
    // When a safety classifier stops the requested model, the CLI answers
    // with a fallback model and still prints `subtype=success`,
    // `is_error=false`. Two signals remain: the fallback model appears as a
    // second `modelUsage` entry, and the text often says it was stopped.
    // The fixtures below are hand-built in the shape the CLI prints.

    /// A success envelope whose `modelUsage` lists `models` (the CLI keys
    /// that object by resolved model id, e.g. `claude-opus-5-5[1m]`).
    fn success_envelope_with_usage(result_text: &str, models: &[&str]) -> String {
        let mut env: serde_json::Value =
            serde_json::from_str(&success_envelope(result_text)).expect("envelope");
        let usage: serde_json::Map<String, serde_json::Value> = models
            .iter()
            .map(|m| {
                (
                    (*m).to_string(),
                    serde_json::json!({"inputTokens": 10, "outputTokens": 20, "costUSD": 0.0}),
                )
            })
            .collect();
        env["modelUsage"] = serde_json::Value::Object(usage);
        env.to_string()
    }

    fn request_failed_message(err: LlmError) -> String {
        match err {
            LlmError::RequestFailed { message } => message,
            other => panic!("expected RequestFailed, got {other:?}"),
        }
    }

    /// (1) Prose saying a safety classifier stopped the answer is a named,
    /// terminal request failure, not a retryable "is not JSON" parse error;
    /// the message carries the stable prefix and none of the model's text.
    #[test]
    fn classifier_stop_with_prose_is_request_failed_not_malformed() {
        let result = "My attempt at that output was stopped by a safety classifier, \
                      so I cannot regenerate the cluster summary.";
        let msg = request_failed_message(
            ClaudeCliProvider::parse_envelope(&success_envelope_with_usage(
                result,
                &["claude-opus-5-5[1m]"],
            ))
            .unwrap_err(),
        );
        assert!(msg.starts_with(CLASSIFIER_STOP_PREFIX), "message: {msg}");
        assert!(
            !msg.contains("cannot regenerate"),
            "the model's text must not reach the error: {msg}"
        );
    }

    /// (1b) The other recorded wording ("a safety filter stopped ...") and
    /// no `modelUsage` at all: the text alone is enough.
    #[test]
    fn safety_filter_wording_is_a_classifier_stop() {
        let result = "A Safety Filter stopped my previous response partway through.";
        let msg = request_failed_message(
            ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
        );
        assert!(msg.starts_with(CLASSIFIER_STOP_PREFIX), "message: {msg}");
    }

    /// (2) A second model in `modelUsage` is a fallback after a stop: even a
    /// result that parses (here plain JSON) is not accepted as the requested
    /// model's answer.
    #[test]
    fn foreign_model_in_model_usage_is_classifier_stop() {
        let result = "{\"title\": \"A\", \"summary\": \"B\"}";
        for models in [
            &["claude-opus-5-5[1m]", "claude-opus-5"][..],
            &["claude-opus-5-5", "claude-opus-5"][..],
        ] {
            let msg = request_failed_message(
                ClaudeCliProvider::parse_envelope(&success_envelope_with_usage(result, models))
                    .unwrap_err(),
            );
            assert!(msg.starts_with(CLASSIFIER_STOP_PREFIX), "{models:?}: {msg}");
        }
    }

    /// (2b) Guard: ONE model, reported under its resolved id with a context
    /// suffix, is a normal call and parses. (Matching keys against an alias
    /// such as `--model opus` would fail every call.)
    #[test]
    fn a_single_model_usage_entry_is_a_normal_call() {
        let result = "{\"title\": \"A\", \"summary\": \"B\"}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope_with_usage(
            result,
            &["claude-opus-5-5[1m]"],
        ))
        .expect("one model in modelUsage is not a stop");
        assert_eq!(v, serde_json::json!({"title": "A", "summary": "B"}));
    }

    /// (3) An empty (or whitespace-only) `result` is its own request
    /// failure, not "is not JSON: EOF while parsing".
    #[test]
    fn empty_result_is_request_failed() {
        for result in ["", "  \n "] {
            let msg = request_failed_message(
                ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
            );
            assert_eq!(msg, "claude -p returned an empty result", "{result:?}");
        }
    }

    /// (4) Prose followed by an UNFENCED JSON object parses to that object
    /// (hardening; there is no classifier signal here).
    #[test]
    fn prose_then_unfenced_json_parses() {
        let result = "Here is the answer:\n{\"title\":\"A\",\"summary\":\"B\"}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope_with_usage(
            result,
            &["claude-sonnet-5"],
        ))
        .expect("unfenced JSON after prose must parse");
        assert_eq!(v, serde_json::json!({"title": "A", "summary": "B"}));
    }

    /// (4b) The same for an array, and for a sliced object that also needs
    /// the raw-newline repair.
    #[test]
    fn prose_then_unfenced_array_or_repairable_object_parses() {
        let v = ClaudeCliProvider::parse_envelope(&success_envelope("Result:\n[1, 2, 3]\nDone."))
            .expect("unfenced array after prose must parse");
        assert_eq!(v, serde_json::json!([1, 2, 3]));

        let result =
            "I'll produce the summary fresh.\n{\"title\": \"A\", \"summary\": \"one\ntwo\",}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope(result))
            .expect("a sliced object is repaired like any other candidate");
        assert_eq!(v, serde_json::json!({"title": "A", "summary": "one\ntwo"}));
    }

    /// (5) Guard: prose that names the classifier and THEN carries a
    /// parseable JSON object is still the classifier stop. Extracting JSON
    /// from around the prose must never launder a stopped answer.
    #[test]
    fn classifier_mention_before_unfenced_or_fenced_json_is_still_a_stop() {
        for result in [
            "The response was stopped by a safety classifier. Here is a fresh one:\n{\"title\":\"A\",\"summary\":\"B\"}",
            "A safety filter stopped my previous answer.\n```json\n{\"title\":\"A\",\"summary\":\"B\"}\n```",
        ] {
            let msg = request_failed_message(
                ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
            );
            assert!(msg.starts_with(CLASSIFIER_STOP_PREFIX), "{result:?}: {msg}");
        }
    }

    /// (6) Guard: a well-formed JSON answer whose CONTENT mentions a safety
    /// filter (a narrative about one) is an answer, not a stop: the text
    /// signal applies only to output that is not JSON as a whole.
    #[test]
    fn json_whose_content_mentions_a_safety_filter_still_parses() {
        let result = "{\"title\": \"Water treatment\", \"summary\": \"The safety filter removes 99% of spores.\"}";
        let v = ClaudeCliProvider::parse_envelope(&success_envelope_with_usage(
            result,
            &["claude-sonnet-5"],
        ))
        .expect("JSON content is not a classifier signal");
        assert_eq!(v["title"], "Water treatment");
    }

    /// (6b) Guard: the same holds for JSON EXTRACTED from around prose. A
    /// fenced block, or an unfenced object after an innocuous prefix, whose
    /// string content mentions a safety filter / classifier is an answer:
    /// the text signal looks only at the prose outside the parsed JSON.
    #[test]
    fn extracted_json_whose_content_mentions_a_safety_filter_still_parses() {
        for result in [
            "```json\n{\"title\":\"Water treatment\",\"summary\":\"The safety filter removes 99% of spores.\"}\n```",
            "Here is the summary:\n{\"title\":\"Water treatment\",\"summary\":\"An answer cut off by a safety classifier is reported.\"}",
            "Result:\n[{\"title\":\"Water treatment\",\"note\":\"safety filter\"}]\nDone.",
        ] {
            let v = ClaudeCliProvider::parse_envelope(&success_envelope_with_usage(
                result,
                &["claude-opus-5-5[1m]"],
            ))
            .unwrap_or_else(|e| panic!("{result:?} must parse, got {e:?}"));
            let title = if v.is_array() { &v[0]["title"] } else { &v["title"] };
            assert_eq!(title, "Water treatment", "{result:?}");
        }
    }

    /// (5b) Guard: a mention AFTER the JSON (prose suffix) is still a stop,
    /// for both an unfenced object and a fenced block.
    #[test]
    fn classifier_mention_after_extracted_json_is_still_a_stop() {
        for result in [
            "{\"title\":\"A\",\"summary\":\"B\"}\nNote: a safety filter stopped part of this answer.",
            "```json\n{\"title\":\"A\",\"summary\":\"B\"}\n```\nThe Safety Classifier interrupted the first attempt.",
        ] {
            let msg = request_failed_message(
                ClaudeCliProvider::parse_envelope(&success_envelope(result)).unwrap_err(),
            );
            assert!(msg.starts_with(CLASSIFIER_STOP_PREFIX), "{result:?}: {msg}");
        }
    }

    #[test]
    fn model_name_reflects_override() {
        std::env::set_var("EPISCIENCE_LLM_MODEL", "claude-opus-4-8");
        let p = ClaudeCliProvider::from_env();
        assert_eq!(p.model_name(), "claude-opus-4-8");
        std::env::remove_var("EPISCIENCE_LLM_MODEL");
    }

    /// What a fake `claude` binary recorded about how `complete_json`
    /// spawned it: one argv entry per element, the working directory, and
    /// that directory's octal permission bits.
    #[cfg(unix)]
    struct Spawned {
        argv: Vec<String>,
        cwd: std::path::PathBuf,
        cwd_mode: String,
    }

    /// Run `complete_json` against a fake `claude` that records its argv and
    /// working directory, then prints a success envelope. Exercises the real
    /// spawn path (no network, no real CLI).
    #[cfg(unix)]
    async fn spawn_recorded(model: Option<&str>) -> Spawned {
        use std::os::unix::fs::PermissionsExt;

        let rec = tempfile::tempdir().expect("record dir");
        let script = rec.path().join("fake-claude");
        let out = rec.path().display().to_string();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 for a in \"$@\"; do printf '%s\\0' \"$a\"; done > '{out}/argv'\n\
                 pwd -P > '{out}/cwd'\n\
                 stat -c %a . > '{out}/mode'\n\
                 printf '%s\\n' '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"{{\\\"ok\\\":true}}\"}}'\n"
            ),
        )
        .expect("write fake claude");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("chmod fake claude");

        let provider = ClaudeCliProvider {
            binary: script.display().to_string(),
            model: model.map(str::to_string),
            timeout: Duration::from_secs(30),
        };
        let v = provider
            .complete_json("Summarize these claims.")
            .await
            .expect("fake claude returns a success envelope");
        assert_eq!(v, serde_json::json!({"ok": true}));

        let raw = std::fs::read(rec.path().join("argv")).expect("argv recorded");
        // NUL-terminated entries; empties are kept so the `""` after `--tools`
        // is checked exactly. The last split element is the empty remainder
        // after the final terminator.
        let mut argv: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|s| String::from_utf8(s.to_vec()).expect("utf8 argv"))
            .collect();
        argv.pop();
        let cwd = std::path::PathBuf::from(
            std::fs::read_to_string(rec.path().join("cwd"))
                .expect("cwd recorded")
                .trim(),
        );
        let cwd_mode = std::fs::read_to_string(rec.path().join("mode"))
            .expect("mode recorded")
            .trim()
            .to_string();
        Spawned {
            argv,
            cwd,
            cwd_mode,
        }
    }

    /// The value that immediately follows `flag` in `argv`, if any.
    #[cfg(unix)]
    fn value_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|a| a == flag)
            .and_then(|i| argv.get(i + 1))
            .map(String::as_str)
    }

    /// The worker's `claude -p` must not inherit the operator's Claude Code
    /// configuration: no user-level settings (which carry plugins, hooks, the
    /// advisor and connector config) and no MCP servers at all. `--tools ""`
    /// alone removes built-in tools only, not MCP tools.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_isolates_the_cli_from_operator_settings_and_mcp() {
        let s = spawn_recorded(None).await;
        assert!(
            s.argv.iter().any(|a| a == "--strict-mcp-config"),
            "argv must carry --strict-mcp-config so no MCP server (claude.ai connectors included) loads; argv: {:?}",
            s.argv
        );
        assert!(
            !s.argv
                .iter()
                .any(|a| a == "--mcp-config" || a.starts_with("--mcp-config=")),
            "strict mode with no --mcp-config means zero MCP servers; argv: {:?}",
            s.argv
        );
        assert_eq!(
            value_after(&s.argv, "--setting-sources"),
            Some("project,local"),
            "user settings must not load; argv: {:?}",
            s.argv
        );
        assert_eq!(
            value_after(&s.argv, "--tools"),
            Some(""),
            "built-in tools stay disabled; argv: {:?}",
            s.argv
        );
    }

    /// An `EPISCIENCE_LLM_MODEL` override adds `--model` and must not displace
    /// any isolation flag.
    #[cfg(unix)]
    #[tokio::test]
    async fn model_override_keeps_every_isolation_flag() {
        let s = spawn_recorded(Some("claude-opus-4-8")).await;
        assert_eq!(value_after(&s.argv, "--model"), Some("claude-opus-4-8"));
        assert!(
            s.argv.iter().any(|a| a == "--strict-mcp-config"),
            "argv: {:?}",
            s.argv
        );
        assert_eq!(
            value_after(&s.argv, "--setting-sources"),
            Some("project,local"),
            "argv: {:?}",
            s.argv
        );
        assert_eq!(
            value_after(&s.argv, "--tools"),
            Some(""),
            "argv: {:?}",
            s.argv
        );
        // The isolation flags close the argv, after every configurable
        // value: the same order the measured deploy wrapper produced.
        assert_eq!(
            s.argv[s.argv.len() - 3..],
            ["--setting-sources", "project,local", "--strict-mcp-config"],
            "argv: {:?}",
            s.argv
        );
    }

    /// The CLI runs from a private, empty directory: never the service's own
    /// working directory (where project settings / CLAUDE.md could be picked
    /// up and where a stray file was once written) and never a world-writable
    /// one.
    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_runs_from_a_private_working_directory() {
        let s = spawn_recorded(None).await;
        let service_cwd = std::env::current_dir()
            .and_then(|p| p.canonicalize())
            .expect("test cwd");
        assert_ne!(
            s.cwd, service_cwd,
            "claude -p must not run in the service's working directory"
        );
        assert_eq!(
            s.cwd_mode,
            "700",
            "working directory {} must be private (0700), not group/world accessible",
            s.cwd.display()
        );
        assert!(
            !s.cwd.exists(),
            "the per-call working directory {} must be removed after the call",
            s.cwd.display()
        );
    }

    /// A relative `EPISCIENCE_CLAUDE_BIN` with a directory part (`bin/claude`)
    /// names a file relative to the service's working directory: that is
    /// where `is_active` looks for it, so the spawn must find the same file,
    /// not resolve it against the empty per-call directory.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_relative_binary_path_spawns_the_file_is_active_found() {
        use std::os::unix::fs::PermissionsExt;

        let here = std::env::current_dir().expect("test cwd");
        let dir = tempfile::tempdir_in(&here).expect("dir under the service cwd");
        let script = dir.path().join("fake-claude");
        std::fs::write(
            &script,
            "#!/bin/sh\n\
             printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"{\\\"ok\\\":true}\"}'\n",
        )
        .expect("write fake claude");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("chmod fake claude");
        let relative = format!(
            "{}/fake-claude",
            dir.path()
                .file_name()
                .expect("tempdir name")
                .to_str()
                .expect("utf8 tempdir name")
        );

        let provider = ClaudeCliProvider {
            binary: relative.clone(),
            model: None,
            timeout: Duration::from_secs(30),
        };
        assert!(
            provider.is_active(),
            "{relative} exists relative to the service cwd, so the provider is active"
        );
        let v = provider.complete_json("Summarize these claims.").await;
        assert_eq!(
            v.expect("an active provider must spawn the binary is_active found"),
            serde_json::json!({"ok": true})
        );
    }

    /// A call that times out must not leave its `claude -p` child running
    /// (spending quota, in a working directory that is about to be removed):
    /// the child is killed when the call gives up on it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timed_out_child_is_killed_not_orphaned() {
        use std::os::unix::fs::PermissionsExt;

        let rec = tempfile::tempdir().expect("record dir");
        let script = rec.path().join("fake-claude");
        let out = rec.path().display().to_string();
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > '{out}/pid'\nexec sleep 30\n"),
        )
        .expect("write fake claude");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("chmod fake claude");

        let provider = ClaudeCliProvider {
            binary: script.display().to_string(),
            model: None,
            timeout: Duration::from_secs(1),
        };
        let err = provider
            .complete_json("Summarize these claims.")
            .await
            .expect_err("a child that outlives the timeout fails the call");
        assert!(
            matches!(&err, LlmError::RequestFailed { message } if message.contains("timed out")),
            "a timeout is a RequestFailed naming the timeout: {err:?}"
        );

        let pid = std::fs::read_to_string(rec.path().join("pid"))
            .expect("the fake recorded its pid")
            .trim()
            .to_string();
        // Dead = gone from /proc or a zombie awaiting its reaper (state Z).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let dead = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => true,
                Ok(stat) => stat
                    .rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with('Z')),
            };
            if dead {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "claude -p child {pid} is still running 5 s after the call timed out: orphaned"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
