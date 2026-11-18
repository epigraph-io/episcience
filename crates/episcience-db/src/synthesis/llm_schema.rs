//! The JSON Schemas the synthesis pipeline's LLM stages pass to
//! `LlmProvider::complete_json_schema` (the kernel trait,
//! `epigraph_interfaces::LlmProvider`, re-exported by
//! `epigraph_cli::enrichment::llm_client`).
//!
//! A provider that can constrain its answer to a schema (the `claude -p
//! --json-schema` CLI) overrides that method and enforces it; the kernel's
//! default ignores the schema and calls `complete_json`. The stage validators
//! still check every field, so a provider that ignores the schema is no less
//! safe. A wrapper that delegates to an inner provider must forward
//! `complete_json_schema` itself, or the default drops the schema.

/// The longest cluster title the database stores, in characters:
/// `synthesis_clusters_title_check` is `length(title) <= 200`, and Postgres
/// `length()` counts characters, as JSON Schema `maxLength` does.
pub const NARRATION_TITLE_MAX_CHARS: usize = 200;

/// The Stage 4 (narrate) answer: a non-empty `title` of at most
/// [`NARRATION_TITLE_MAX_CHARS`] characters, and a non-empty `summary`.
pub fn narration_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "minLength": 1,
                "maxLength": NARRATION_TITLE_MAX_CHARS
            },
            "summary": { "type": "string", "minLength": 1 }
        },
        "required": ["title", "summary"]
    })
}

/// The Stage 5 (compose) answer: a non-empty `narrative`.
pub fn composition_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "narrative": { "type": "string", "minLength": 1 }
        },
        "required": ["narrative"]
    })
}
