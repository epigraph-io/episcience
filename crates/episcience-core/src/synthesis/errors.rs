use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum SynthesisError {
    #[error("seed recall returned no claims for query")]
    EmptyResult,
    #[error("traversal subgraph exceeded max_subgraph_size")]
    SubgraphTooLarge,
    #[error("LLM hallucinated claim id not in cluster: {0}")]
    HallucinatedClaimId(Uuid),
    #[error("narration left a cluster member uncited by its full id: {0}")]
    UncitedMember(Uuid),
    #[error("narration cited cluster member {claim_id} by the abbreviated id [{token}]")]
    AbbreviatedCitation { token: String, claim_id: Uuid },
    #[error("compose stage violated cluster anchor: {cluster_id}")]
    ComposeAnchorViolation { cluster_id: Uuid },
    #[error("LLM call budget exceeded: limit {limit}")]
    CostBudgetExceeded { limit: u32 },
    #[error("LLM error: {0}")]
    Llm(String),
    /// The model (or a safety classifier in front of it) declined to answer:
    /// the provider's `LlmError::Refusal`, carrying its message. A refusal
    /// depends on the prompt's content, so it is terminal for the whole job,
    /// not only for the prompt: re-running the job re-sends the same prompts
    /// and pays for the same answer. Kept apart from [`Self::Llm`] (transport
    /// or parse failures, which a later attempt can get past) so the job
    /// layer can tell the two apart without parsing a message.
    #[error("LLM refused the request: {0}")]
    Refused(String),
    #[error("epigraph edge write failed: {0}")]
    EdgeWrite(String),
    #[error("database error: {0}")]
    Db(String),
    #[error("validation: {0}")]
    Validation(String),
}
