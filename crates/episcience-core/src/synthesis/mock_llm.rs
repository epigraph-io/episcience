//! Test-only recording wrapper around any LlmClient. Exposed via the
//! `test-utils` feature so phase-2/phase-5 integration tests in other
//! crates can pull it in.
//!
//! NOTE: The real `LlmClient` trait (epigraph-cli) uses:
//!   `complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError>`
//!   `model_name(&self) -> &str`
//! The plan's prototype used `complete -> anyhow::Result<String>`; this impl
//! mirrors the actual upstream trait instead.

use std::fmt;
use std::sync::{Arc, Mutex};

use epigraph_cli::enrichment::llm_client::{LlmError, LlmProvider};

#[derive(Debug, Clone)]
pub struct LlmCall {
    pub prompt: String,
    pub response: serde_json::Value,
}

pub struct RecordingLlmClient<L> {
    inner: Arc<L>,
    log: Arc<Mutex<Vec<LlmCall>>>,
}

impl<L: fmt::Debug> fmt::Debug for RecordingLlmClient<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordingLlmClient")
            .field("inner", &self.inner)
            .field("log_len", &self.log.lock().unwrap().len())
            .finish()
    }
}

impl<L> RecordingLlmClient<L> {
    pub fn new(inner: Arc<L>) -> Self {
        Self {
            inner,
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn recorded(&self) -> Vec<LlmCall> {
        self.log.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.log.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl<L> LlmProvider for RecordingLlmClient<L>
where
    L: LlmProvider + Send + Sync + fmt::Debug,
{
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    async fn complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError> {
        let response = self.inner.complete_json(prompt).await?;
        self.log.lock().unwrap().push(LlmCall {
            prompt: prompt.to_string(),
            response: response.clone(),
        });
        Ok(response)
    }

    /// Forwards to the inner provider's `complete_json_schema`, so the schema
    /// reaches it; the kernel default would call `complete_json` and drop it.
    async fn complete_json_schema(
        &self,
        prompt: &str,
        schema: &serde_json::Value,
    ) -> Result<serde_json::Value, LlmError> {
        let response = self.inner.complete_json_schema(prompt, schema).await?;
        self.log.lock().unwrap().push(LlmCall {
            prompt: prompt.to_string(),
            response: response.clone(),
        });
        Ok(response)
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Debug)]
    struct EchoLlm;

    #[async_trait::async_trait]
    impl LlmProvider for EchoLlm {
        fn name(&self) -> &str {
            "echo"
        }

        fn is_active(&self) -> bool {
            true
        }

        async fn complete_json(&self, prompt: &str) -> Result<serde_json::Value, LlmError> {
            Ok(serde_json::json!({"echo": prompt}))
        }

        fn model_name(&self) -> &str {
            "echo"
        }
    }

    /// Answers `complete_json_schema` with the schema it received, and
    /// `complete_json` with a marker, so a caller can tell which ran.
    #[derive(Debug)]
    struct SchemaEchoLlm;

    #[async_trait::async_trait]
    impl LlmProvider for SchemaEchoLlm {
        fn name(&self) -> &str {
            "schema-echo"
        }

        fn is_active(&self) -> bool {
            true
        }

        async fn complete_json(&self, _prompt: &str) -> Result<serde_json::Value, LlmError> {
            Ok(serde_json::json!({"schema": null}))
        }

        async fn complete_json_schema(
            &self,
            _prompt: &str,
            schema: &serde_json::Value,
        ) -> Result<serde_json::Value, LlmError> {
            Ok(serde_json::json!({"schema": schema}))
        }

        fn model_name(&self) -> &str {
            "schema-echo"
        }
    }

    /// The recorder forwards `complete_json_schema` to the provider it wraps,
    /// schema included, and records the call. Kills: the kernel default on
    /// the recorder, which calls the inner `complete_json` and silently
    /// drops the schema.
    #[tokio::test]
    async fn recording_llm_forwards_the_schema_and_records_the_call() {
        let recorder = RecordingLlmClient::new(Arc::new(SchemaEchoLlm));
        let schema = serde_json::json!({"type": "object", "required": ["title"]});
        let answer = recorder
            .complete_json_schema("narrate", &schema)
            .await
            .unwrap();
        assert_eq!(answer, serde_json::json!({"schema": schema}));
        let log = recorder.recorded();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].prompt, "narrate");
        assert_eq!(log[0].response, answer);
    }

    #[tokio::test]
    async fn recording_llm_records_prompt_and_response_pairs() {
        let inner = Arc::new(EchoLlm);
        let recorder = RecordingLlmClient::new(inner);
        let _ = recorder.complete_json("first").await.unwrap();
        let _ = recorder.complete_json("second").await.unwrap();
        let log = recorder.recorded();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].prompt, "first");
        assert_eq!(log[0].response["echo"], "first");
        assert_eq!(log[1].prompt, "second");
        assert_eq!(log[1].response["echo"], "second");
    }
}
