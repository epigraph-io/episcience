//! Structured-output extension of the kernel [`LlmProvider`].
//!
//! The kernel trait (pinned `epigraph-interfaces`) has only
//! `complete_json(&self, prompt)`, so a provider that can constrain its
//! answer to a JSON schema (the `claude -p --json-schema` CLI) has no way to
//! receive one, and a stage's output shape rests on prose instructions alone.
//! [`SchemaLlm`] adds `complete_json_schema`, whose default ignores the
//! schema and calls `complete_json`, so every existing provider opts in with
//! an empty `impl`. The synthesis pipeline's LLM stages are bounded on it and
//! pass [`narration_schema`] / [`composition_schema`]; the stage validators
//! still check every field, so a provider that ignores the schema is no less
//! safe than before.
//!
//! Follow-up for the next kernel pin bump: the same method as a defaulted
//! method on `epigraph_interfaces::LlmProvider` (non-breaking for its
//! implementors), after which this trait can go.

use async_trait::async_trait;
use epigraph_cli::enrichment::llm_client::{AnthropicClient, LlmError, LlmProvider, MockLlmClient};

/// An [`LlmProvider`] that may constrain its JSON answer to a schema.
#[allow(clippy::double_must_use)] // async_trait's generated #[must_use] on an already-must-use boxed future
#[async_trait]
pub trait SchemaLlm: LlmProvider {
    /// Like [`LlmProvider::complete_json`], with `schema` (a JSON Schema
    /// object) describing the answer. A provider that can enforce it should;
    /// the default ignores it and calls `complete_json`. Callers must still
    /// validate the answer.
    async fn complete_json_schema(
        &self,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> Result<serde_json::Value, LlmError> {
        let _ = schema;
        self.complete_json(prompt).await
    }
}

// Kernel providers cannot enforce a schema: they take the default.
impl SchemaLlm for MockLlmClient {}
impl SchemaLlm for AnthropicClient {}

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
