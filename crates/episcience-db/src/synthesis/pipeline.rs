//! Synthesis pipeline — Stages 1 and 2.
//!
//! Wires together the seed → traverse → cluster → compose → validate → narrate
//! pipeline. Phase 2 / Tasks 2.2 + 2.3 implement Stages 1 and 2; subsequent
//! stages land in later tasks.
//!
//! # Stage 1 — Seed
//!
//! Calls [`epigraph_engine::recall::recall`] with the user's natural-language
//! query and returns a `Vec<Uuid>` of seed claim ids. Empty results are
//! surfaced as [`SynthesisError::EmptyResult`] so the caller can short-circuit
//! before traversal.
//!
//! # Stage 2 — Traverse + persist
//!
//! BFS traversal over the trust-edge graph using the configured edge provider,
//! relevance-pruning by cosine similarity against the precomputed query
//! embedding. Persists the resulting [`SubgraphSnapshot`] (incl. per-claim
//! Bel/Pl/BetP intervals) and the `synthesis_claim_membership` rows in a
//! single transaction so the two never diverge.
//!
//! Note: `recall::recall` returns `RecallResult.claim_id: String` (the
//! UUID's string form). We parse back to `Uuid` here so downstream stages get
//! the typed id directly. A parse failure means the upstream library returned
//! a malformed id and is treated as a validation error.

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use episcience_core::synthesis::clustering;
use episcience_core::synthesis::errors::SynthesisError;
use episcience_core::synthesis::traversal::{self, EdgeProvider, EdgeType, TraversalConfig};
use episcience_core::synthesis::{BeliefIntervalEntry, Cluster, SubgraphSnapshot};

use crate::{SynthesisClustersRepository, SynthesisMembershipRepository, SynthesisRepository};

/// End-to-end synthesis pipeline.
///
/// Generic over the LLM client and edge provider so tests can inject mocks
/// without depending on the production transport stack. The embedder is held
/// behind a trait object because [`epigraph_embeddings::EmbeddingService`] is
/// already a trait-object-friendly `Send + Sync` interface.
///
/// `query_embedding` is the precomputed embedding of the user's natural-language
/// query, used by Stage 2 traversal to score relevance of candidate neighbours
/// via cosine similarity. Stage 1 doesn't read it; callers that only run
/// Stage 1 can pass `vec![]`.
///
/// `subgraph_metadata` is a free-form JSON value passed into Stage 4's narrate
/// prompt (e.g., aggregate Bel/Pl summary, frame info). Initialised to `{}` by
/// [`Self::new`]; callers can mutate it in place before running Stage 4.
///
/// `llm_call_count` is the running counter of LLM calls made by this pipeline
/// instance, enforced against `cost_budget` by [`Self::call_llm_with_retry`].
/// `cost_budget` is the per-synthesis hard cap (spec §"Risks", default 20).
pub struct SynthesisPipeline<L, P> {
    pub pool: PgPool,
    pub embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
    pub llm_client: L,
    pub edge_provider: P,
    pub query_embedding: Vec<f32>,
    pub subgraph_metadata: serde_json::Value,
    pub llm_call_count: u32,
    pub cost_budget: u32,
}

impl<L, P> SynthesisPipeline<L, P> {
    /// Construct a new pipeline with the supplied dependencies.
    ///
    /// `query_embedding` should be the embedding of the user's query as
    /// produced by [`epigraph_embeddings::EmbeddingService::generate_query`].
    /// Pass `vec![]` if you only intend to call [`Self::stage1_seed`] — Stage
    /// 1 doesn't read it.
    ///
    /// `cost_budget` is the per-pipeline cap on LLM calls (spec default: 20).
    /// `subgraph_metadata` is initialised to `{}`; callers may overwrite the
    /// `pub` field before running Stage 4 if richer prompt context is wanted.
    pub fn new(
        pool: PgPool,
        embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
        llm_client: L,
        edge_provider: P,
        query_embedding: Vec<f32>,
        cost_budget: u32,
    ) -> Self {
        Self {
            pool,
            embedder,
            llm_client,
            edge_provider,
            query_embedding,
            subgraph_metadata: serde_json::json!({}),
            llm_call_count: 0,
            cost_budget,
        }
    }
}

// Stage 1 needs no `LlmClient` / `EdgeProvider` bounds — it only touches
// `self.pool` and `self.embedder`. Keeping this impl unbounded lets
// `epigraph-cli` stay an optional dependency (gated by the `test-utils`
// feature). Future stages that DO call `self.llm_client` or
// `self.edge_provider` will live in their own `impl` blocks with the bounds
// they actually need (likely re-exporting `LlmClient` through
// `episcience_core` so the trait is visible without the cli dep).
impl<L, P> SynthesisPipeline<L, P> {
    /// Stage 1 — Seed.
    ///
    /// Calls `epigraph_engine::recall::recall` and returns the parsed seed
    /// claim ids. An empty result set is mapped to
    /// [`SynthesisError::EmptyResult`] (not `Ok(vec![])`) so the runner can
    /// fail fast before traversal.
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::EmptyResult`] — recall returned zero results.
    /// - [`SynthesisError::Db`] — recall's database / fallback path failed.
    /// - [`SynthesisError::Validation`] — recall returned a malformed UUID
    ///   string (should not happen with the upstream contract; defensive).
    pub async fn stage1_seed(
        &self,
        query: &str,
        limit: usize,
        min_truth: f64,
    ) -> Result<Vec<Uuid>, SynthesisError> {
        let results = epigraph_engine::recall::recall(
            &self.pool,
            self.embedder.as_ref(),
            query,
            limit,
            min_truth,
        )
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;

        if results.is_empty() {
            return Err(SynthesisError::EmptyResult);
        }

        results
            .into_iter()
            .map(|r| {
                Uuid::parse_str(&r.claim_id).map_err(|e| {
                    SynthesisError::Validation(format!(
                        "recall returned malformed claim_id {:?}: {}",
                        r.claim_id, e
                    ))
                })
            })
            .collect()
    }
}

impl<L, P> SynthesisPipeline<L, P>
where
    P: EdgeProvider,
{
    /// Stage 2 — Traverse + persist.
    ///
    /// BFS over the trust-edge graph from `seeds`, pruning candidate neighbours
    /// whose stored embedding's cosine similarity against `self.query_embedding`
    /// is below `cfg.relevance_prune`. For each surviving claim we compute (or
    /// look up cached) Bel/Pl/BetP via [`epigraph_engine::belief_query::get_belief`]
    /// in the unframed mode (frame_id = `None`).
    ///
    /// The resulting [`SubgraphSnapshot`] and the corresponding
    /// `synthesis_claim_membership` rows are persisted in a single transaction
    /// so the two stay consistent: either both succeed or neither does.
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::Db`] — any database error during traversal,
    ///   belief lookup, or persistence.
    pub async fn stage2_traverse(
        &self,
        synthesis_id: Uuid,
        seeds: Vec<Uuid>,
        cfg: &TraversalConfig,
    ) -> Result<SubgraphSnapshot, SynthesisError> {
        // 1. BFS via traversal::traverse with relevance closure that fetches
        //    stored claim embedding via EmbeddingService::get and computes
        //    cosine vs self.query_embedding.
        let q_embed = self.query_embedding.clone();
        let embedder = self.embedder.clone();
        let mut snapshot = traversal::traverse(&seeds, cfg, &self.edge_provider, |c| {
            let q = q_embed.clone();
            let e = embedder.clone();
            async move {
                match e.get(c).await {
                    Ok(c_embed) => episcience_core::synthesis::util::cosine(&q, &c_embed),
                    // Claim has no stored embedding → conservative prune.
                    Err(_) => 0.0,
                }
            }
        })
        .await?;

        // 2. Per-claim get_belief (unframed; frame_id = None).
        for cid in &snapshot.claim_ids {
            let bi = epigraph_engine::belief_query::get_belief(&self.pool, *cid, None)
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
            snapshot.belief_intervals.push(BeliefIntervalEntry {
                claim_id: *cid,
                frame_id: None,
                belief: bi.belief,
                plausibility: bi.plausibility,
                pignistic_prob: bi.pignistic_prob,
                framed: bi.framed,
            });
        }

        // 3. Persist snapshot + membership in one transaction.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
        SynthesisRepository::save_snapshot_tx(&mut tx, synthesis_id, &snapshot)
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
        SynthesisMembershipRepository::replace_for_synthesis(
            &mut tx,
            synthesis_id,
            &snapshot.claim_ids,
        )
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
        tx.commit()
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;

        Ok(snapshot)
    }
}

// Stage 3 needs no `LlmClient` / `EdgeProvider` bounds — it consumes a
// pre-built `SubgraphSnapshot` and a typed edge list, runs the pure
// `clustering::cluster_signed` function, and persists rows. Keeping this in
// its own unbounded `impl` block matches Stage 1's pattern.
impl<L, P> SynthesisPipeline<L, P> {
    /// Stage 3 — Cluster.
    ///
    /// Maps `edges_with_types` to signed weights:
    ///   SUPPORTS / CORROBORATES → +1.0
    ///   METHODOLOGY             → +0.5
    ///   CONTRADICTS             → −0.5
    ///   SUPERSEDES              →  0.0
    ///
    /// Runs [`clustering::cluster_signed`] (positive Louvain + post-hoc
    /// CONTRADICTS separation + merge-cap at 12) over the snapshot's claim
    /// ids. For each resulting member set, builds a [`Cluster`] with empty
    /// `title` / `summary` (Stage 4 narrates them) and counts of within-cluster
    /// positive vs negative edges, then persists each via
    /// [`SynthesisClustersRepository::insert`].
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::Db`] — any insert failure.
    pub async fn stage3_cluster(
        &self,
        synthesis_id: Uuid,
        snapshot: &SubgraphSnapshot,
        edges_with_types: &[(Uuid, Uuid, EdgeType)],
    ) -> Result<Vec<Cluster>, SynthesisError> {
        let signed: Vec<(Uuid, Uuid, f64)> = edges_with_types
            .iter()
            .map(|(a, b, t)| {
                let w = match t {
                    EdgeType::Supports | EdgeType::Corroborates => 1.0,
                    EdgeType::Methodology => 0.5,
                    EdgeType::Contradicts => -0.5,
                    EdgeType::Supersedes => 0.0,
                };
                (*a, *b, w)
            })
            .collect();

        let raw = clustering::cluster_signed(&snapshot.claim_ids, &signed, 12);

        let mut clusters = Vec::new();
        for (i, members) in raw.into_iter().enumerate() {
            let support_count = signed
                .iter()
                .filter(|(a, b, w)| *w > 0.0 && members.contains(a) && members.contains(b))
                .count() as i32;
            let contradict_count = signed
                .iter()
                .filter(|(a, b, w)| *w < 0.0 && members.contains(a) && members.contains(b))
                .count() as i32;
            let cluster = Cluster {
                id: Uuid::now_v7(),
                synthesis_id,
                cluster_index: i as i32,
                title: String::new(),   // populated in Stage 4
                summary: String::new(), // populated in Stage 4
                member_claim_ids: members,
                support_count,
                contradict_count,
            };
            SynthesisClustersRepository::insert(&self.pool, &cluster)
                .await
                .map_err(|e| SynthesisError::Db(e.to_string()))?;
            clusters.push(cluster);
        }

        Ok(clusters)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Stage 4 — Narrate (per-cluster)
// ──────────────────────────────────────────────────────────────────────────────

/// Build the per-cluster narrate prompt.
///
/// The LLM is asked to return strict JSON `{title, summary}`. The summary may
/// cite member claims using `[<uuid>]` brackets — every such citation MUST be
/// one of the cluster's `member_claim_ids`. Stage 4's validator enforces that
/// invariant; the prompt restates it so the LLM is less likely to violate it
/// in the first place.
fn build_narrate_prompt(c: &Cluster, _meta: &serde_json::Value) -> String {
    let ids: Vec<String> = c.member_claim_ids.iter().map(|u| u.to_string()).collect();
    format!(
        "You are summarizing a cluster of related claims for a synthesis report.\n\
         Cluster id: {}\n\
         Cluster index: {}\n\
         Member claim ids (use these EXACTLY when citing): {:?}\n\n\
         Return strict JSON with this shape:\n\
         {{\n  \"title\": \"<short title, <= 200 chars>\",\n  \"summary\": \"<1-3 paragraph summary that cites claims as [<uuid>] inline>\"\n}}\n\n\
         CRITICAL: every [<uuid>] token in the summary MUST be one of the member claim ids above. \
         Do not invent claim ids. Do not cite claims outside this cluster.",
        c.id, c.cluster_index, ids,
    )
}

// Stage 4 needs `LlmClient` to invoke the model and parse responses. Keeping
// the bound scoped to this impl block matches Stages 1-3's pattern of only
// pulling in the constraints each stage actually requires.
impl<L, P> SynthesisPipeline<L, P>
where
    L: epigraph_cli::enrichment::llm_client::LlmClient,
{
    /// Call the LLM with response-validation + bounded retries + cost-budget
    /// enforcement.
    ///
    /// Increments `self.llm_call_count` *before* the call (so a failing call
    /// still counts against the budget). On `Ok(())` from `validator`, returns
    /// the response; on `Err(_)`, retries up to `max_retries` more times. On
    /// the final retry's `Err(_)`, returns the validator's error.
    ///
    /// Returns [`SynthesisError::CostBudgetExceeded`] if the next call would
    /// exceed `self.cost_budget`. Returns [`SynthesisError::Llm`] if the LLM
    /// transport itself errors (those errors are NOT retried — a transport
    /// failure is treated as terminal for this prompt).
    pub async fn call_llm_with_retry<F>(
        &mut self,
        prompt: &str,
        max_retries: u32,
        validator: F,
    ) -> Result<serde_json::Value, SynthesisError>
    where
        F: Fn(&serde_json::Value) -> Result<(), SynthesisError>,
    {
        let mut last_err: Option<SynthesisError> = None;
        for attempt in 0..=max_retries {
            if self.llm_call_count >= self.cost_budget {
                return Err(SynthesisError::CostBudgetExceeded {
                    limit: self.cost_budget,
                });
            }
            self.llm_call_count += 1;
            let response = self
                .llm_client
                .complete_json(prompt)
                .await
                .map_err(|e| SynthesisError::Llm(e.to_string()))?;
            match validator(&response) {
                Ok(()) => return Ok(response),
                Err(e) if attempt < max_retries => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        // Loop above always returns or assigns last_err on every Err branch
        // up to `max_retries`, then returns on the final Err. Defensive
        // fallback — shouldn't be reachable.
        Err(last_err.unwrap_or_else(|| {
            SynthesisError::Llm("call_llm_with_retry exited loop without result".into())
        }))
    }

    /// Stage 4 — Narrate.
    ///
    /// For each cluster, calls the LLM with a per-cluster prompt and validates
    /// that every `[<uuid>]` citation in the returned `summary` is a member of
    /// `c.member_claim_ids`. Hallucinated ids trigger one retry; persistent
    /// hallucination after the retry surfaces as
    /// [`SynthesisError::HallucinatedClaimId`].
    ///
    /// On success, persists `title` / `summary` for each cluster via
    /// [`SynthesisClustersRepository::update_text`] and returns the updated
    /// `Vec<Cluster>` (in the same order as `clusters`).
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::HallucinatedClaimId`] — citation not in cluster.
    /// - [`SynthesisError::CostBudgetExceeded`] — `llm_call_count` >= budget.
    /// - [`SynthesisError::Llm`] — LLM transport failure (not retried).
    /// - [`SynthesisError::Db`] — UPDATE on synthesis_clusters failed.
    pub async fn stage4_narrate(
        &mut self,
        _synthesis_id: Uuid,
        clusters: &[Cluster],
    ) -> Result<Vec<Cluster>, SynthesisError> {
        let mut out = Vec::with_capacity(clusters.len());
        // Compile the citation regex once per stage4_narrate call. It looks
        // for `[<uuid>]` tokens (lowercase hex) in the LLM's summary.
        let cite_re = regex::Regex::new(r"\[([0-9a-f-]{36})\]").expect("static regex");
        for c in clusters {
            let prompt = build_narrate_prompt(c, &self.subgraph_metadata);
            let member_ids = c.member_claim_ids.clone();
            let cite_re_ref = &cite_re;
            let response = self
                .call_llm_with_retry(&prompt, 1, |json| {
                    let summary = json.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                    for cap in cite_re_ref.captures_iter(summary) {
                        let id: Uuid = cap[1].parse().map_err(|_| {
                            // Regex matched a 36-char hex-with-dashes pattern
                            // that didn't parse as a Uuid — treat as a
                            // hallucination with sentinel id.
                            SynthesisError::HallucinatedClaimId(Uuid::nil())
                        })?;
                        if !member_ids.contains(&id) {
                            return Err(SynthesisError::HallucinatedClaimId(id));
                        }
                    }
                    Ok(())
                })
                .await?;
            let title = response["title"].as_str().unwrap_or("").to_string();
            let summary = response["summary"].as_str().unwrap_or("").to_string();
            let updated = Cluster {
                title,
                summary,
                ..c.clone()
            };
            SynthesisClustersRepository::update_text(
                &self.pool,
                updated.id,
                &updated.title,
                &updated.summary,
            )
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))?;
            out.push(updated);
        }
        Ok(out)
    }
}
