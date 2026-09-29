//! The synthesis runtime's model providers, chosen from the environment.
//! Used by `episcience-worker` (the LLM and the embedder) and by the REST
//! server (the embedder its synthesis search embeds queries with), so both
//! pick the same models from the same variables.

use std::sync::Arc;

use epigraph_cli::enrichment::llm_client::{AnthropicClient, LlmProvider, MockLlmClient};
use epigraph_embeddings::{EmbeddingConfig, EmbeddingService, MockProvider, OpenAiProvider};

use crate::clients::claude_cli::ClaudeCliProvider;

/// Embedding dimension used by the synthesis pipeline.
///
/// `synthesis_embeddings.embedding` is `vector(1536)` (migration 5013), and the
/// upstream EpiGraph claim embeddings are also 1536 (text-embedding-3-small).
/// Both providers configured here must produce 1536-dim vectors.
pub const SYNTHESIS_EMBEDDING_DIM: usize = 1536;

/// Embedding model name written to `synthesis_embeddings.embedding_model`.
pub const DEFAULT_EMBEDDING_MODEL: &str = "text-embedding-3-small";

/// The synthesis LLM: `EPISCIENCE_LLM_MODE` (`claude_cli`, or `anthropic`
/// with `ANTHROPIC_API_KEY`), else the deterministic mock.
pub fn llm_from_env() -> Arc<dyn LlmProvider> {
    // ─── LLM client ───────────────────────────────────────────────────────────
    //
    // Default to MockLlmClient unless explicitly opted into Anthropic AND an
    // API key is present. Mock errors are loud and deterministic, which beats
    // a misconfigured production client silently rotating retries.
    let llm_mode = std::env::var("EPISCIENCE_LLM_MODE").unwrap_or_default();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
    match (llm_mode.as_str(), anthropic_key.as_str()) {
        // Preferred real-LLM path: the `claude -p` CLI (OAuth, prepaid Max/Pro,
        // self-refreshing token) — no ANTHROPIC_API_KEY needed. Mirrors the
        // epiclaw-host convention; see `clients::claude_cli`.
        ("claude_cli", _) => {
            let provider = ClaudeCliProvider::from_env();
            if provider.is_active() {
                tracing::info!(
                    model = %provider.model_name(),
                    "Using ClaudeCliProvider (claude -p) for synthesis LLM",
                );
                Arc::new(provider)
            } else {
                tracing::warn!(
                    "EPISCIENCE_LLM_MODE=claude_cli but the `claude` binary is not on PATH; \
                 falling back to MockLlmClient",
                );
                Arc::new(MockLlmClient::new())
            }
        }
        ("anthropic", key) if !key.is_empty() => {
            let model = std::env::var("ANTHROPIC_MODEL").ok();
            match AnthropicClient::new(key.to_string(), model.clone()) {
                Ok(c) => {
                    tracing::info!(
                        model = %model.unwrap_or_else(|| "<default>".to_string()),
                        "Using AnthropicClient for synthesis LLM",
                    );
                    Arc::new(c)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "AnthropicClient init failed; falling back to MockLlmClient",
                    );
                    Arc::new(MockLlmClient::new())
                }
            }
        }
        _ => {
            tracing::info!(
                "Using MockLlmClient for synthesis LLM \
             (set EPISCIENCE_LLM_MODE=anthropic + ANTHROPIC_API_KEY for real LLM)"
            );
            Arc::new(MockLlmClient::new())
        }
    }
}

/// The synthesis embedder: `EPISCIENCE_EMBED_MODE=openai` with
/// `OPENAI_API_KEY`, else the mock provider.
pub fn embedder_from_env() -> Arc<dyn EmbeddingService> {
    // ─── Embedder ─────────────────────────────────────────────────────────────
    //
    // OpenAiProvider only does live API calls when the `openai` feature is
    // enabled in epigraph-embeddings. With the feature off, `generate_query`
    // returns ConfigError on the first call. The handler tolerates that
    // (Stage 2 prunes all neighbours), but for a dev smoke run it's noisy —
    // default to MockProvider unless explicitly opted in AND an API key is
    // present.
    let embed_mode = std::env::var("EPISCIENCE_EMBED_MODE").unwrap_or_default();
    let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    match (embed_mode.as_str(), openai_key.as_str()) {
        ("openai", key) if !key.is_empty() => {
            let cfg = EmbeddingConfig::openai(SYNTHESIS_EMBEDDING_DIM);
            match OpenAiProvider::new(cfg, key.to_string()) {
                Ok(p) => {
                    tracing::info!(
                        dim = SYNTHESIS_EMBEDDING_DIM,
                        "Using OpenAiProvider for synthesis embeddings",
                    );
                    Arc::new(p)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "OpenAiProvider init failed; falling back to MockProvider",
                    );
                    Arc::new(MockProvider::new(EmbeddingConfig::openai(
                        SYNTHESIS_EMBEDDING_DIM,
                    )))
                }
            }
        }
        _ => {
            tracing::info!(
                dim = SYNTHESIS_EMBEDDING_DIM,
                "Using MockProvider for synthesis embeddings \
             (set EPISCIENCE_EMBED_MODE=openai + OPENAI_API_KEY for real embeddings)"
            );
            Arc::new(MockProvider::new(EmbeddingConfig::openai(
                SYNTHESIS_EMBEDDING_DIM,
            )))
        }
    }
}

/// `EPISCIENCE_EMBEDDING_MODEL`, else [`DEFAULT_EMBEDDING_MODEL`].
pub fn embedding_model_from_env() -> String {
    std::env::var("EPISCIENCE_EMBEDDING_MODEL")
        .unwrap_or_else(|_| DEFAULT_EMBEDDING_MODEL.to_string())
}
