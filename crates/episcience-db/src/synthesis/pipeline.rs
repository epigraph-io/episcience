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
    /// Skill that contributes per-stage prompt sections and (in Phase 4)
    /// the verification rubric. Defaults to `BaselineSkill` for
    /// behaviour-preserving construction; callers wanting another skill
    /// use [`Self::with_skill`] after `new`.
    pub skill: Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>,
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
            skill: episcience_core::synthesis::skills::default_skill(),
        }
    }

    /// Replace the skill on a constructed pipeline. Used by the job
    /// handler (Task 2.3) after resolving `syntheses.skill_name` from
    /// the row.
    pub fn with_skill(
        mut self,
        skill: Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>,
    ) -> Self {
        self.skill = skill;
        self
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
///
/// `contents` carries the actual claim text fetched by the Stage 4 driver
/// (see [`fetch_claim_contents`]). Without it, the prompt would only carry
/// UUIDs and the LLM would have no text to summarize — historically the cause
/// of Stage 4 failing on prod with empty/hallucinated outputs. Each entry's
/// content is truncated to 800 chars to keep the prompt within budget.
///
/// If a member id is missing from `contents` (e.g., the upstream claim was
/// deleted between Stage 2 and Stage 4) it is omitted from the claims block;
/// the validator still allows the LLM to cite or omit it.
fn build_narrate_prompt(
    skill_section: &str,
    c: &Cluster,
    contents: &[(Uuid, String)],
    _meta: &serde_json::Value,
) -> String {
    const MAX_CONTENT_CHARS: usize = 800;
    let claims_block: String = if contents.is_empty() {
        String::from("(no claim content available — member ids only)")
    } else {
        contents
            .iter()
            .map(|(id, text)| {
                let truncated: String = text.chars().take(MAX_CONTENT_CHARS).collect();
                format!("[{}] {}", id, truncated)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let id_list: Vec<String> = c.member_claim_ids.iter().map(|u| u.to_string()).collect();
    let intro = if skill_section.is_empty() {
        String::from("You are summarizing a cluster of related claims for a synthesis report.")
    } else {
        format!(
            "You are summarizing a cluster of related claims for a synthesis report.\n\n\
             Skill guidance: {skill_section}"
        )
    };
    format!(
        "{intro}\n\
         Cluster id: {}\n\
         Cluster index: {}\n\
         Member claim ids (use these EXACTLY when citing): {:?}\n\n\
         The claims in this cluster (cite by [<uuid>] when referring to them):\n\n\
         {claims_block}\n\n\
         Return strict JSON with this shape:\n\
         {{\n  \"title\": \"<short title, <= 200 chars>\",\n  \"summary\": \"<1-3 paragraph summary that cites claims as [<uuid>] inline>\"\n}}\n\n\
         CRITICAL: every [<uuid>] token in the summary MUST be one of the member claim ids above. \
         Do not invent claim ids. Do not cite claims outside this cluster.",
        c.id, c.cluster_index, id_list,
    )
}

/// Fetch `(claim_id, content)` rows for the given ids from the upstream
/// `claims` table. Used by Stage 4 to enrich the narrate prompt with actual
/// claim text (was the chief failure mode in the prod e2e — UUIDs alone gave
/// the LLM nothing to summarize).
///
/// Missing ids (claim deleted, or id not in this DB) are silently dropped —
/// the prompt-builder gracefully degrades when a member has no content. We
/// don't fail Stage 4 on a missing claim because the validator already
/// enforces that any cited UUIDs come from `member_claim_ids`; an empty
/// claims-block just means the LLM has less to draw on.
async fn fetch_claim_contents(
    pool: &PgPool,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, String)>, SynthesisError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows =
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, content FROM claims WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await
            .map_err(|e| SynthesisError::Db(format!("fetch_claim_contents: {e}")))?;
    Ok(rows)
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
            // Fetch claim content for this cluster's members so the LLM has
            // something to summarize. A missing claim (deleted upstream)
            // simply yields fewer rows; `build_narrate_prompt` degrades
            // gracefully and the validator still enforces citation safety.
            let contents = fetch_claim_contents(&self.pool, &c.member_claim_ids).await?;
            let section = self
                .skill
                .section(episcience_core::synthesis::skill::SynthesisStage::Narration)
                .unwrap_or("");
            let prompt = build_narrate_prompt(section, c, &contents, &self.subgraph_metadata);
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

    /// Stage 5 — Compose.
    ///
    /// Asks the LLM to weave the per-cluster summaries (already populated by
    /// Stage 4) into a single Markdown narrative answering the user query.
    /// Each cluster's summary must appear VERBATIM in the LLM's response,
    /// wrapped in `<<<CLUSTER:{id}:BEGIN>>> ... <<<CLUSTER:{id}:END>>>`
    /// sentinels. The validator extracts the bytes between each sentinel pair
    /// and compares them byte-for-byte against `cluster.summary`; any
    /// modification, omitted sentinel, or sentinel reordering surfaces as
    /// [`SynthesisError::ComposeAnchorViolation`] (with one retry).
    ///
    /// The returned narrative has the sentinel markers stripped — callers
    /// receive clean Markdown ready for downstream use. Stage 5 does NOT touch
    /// the database; persistence happens in Stage 6.
    ///
    /// `synthesis_id` is accepted for symmetry with the other stages and is
    /// currently unused inside the body.
    ///
    /// # Errors
    ///
    /// - [`SynthesisError::ComposeAnchorViolation`] — sentinel missing,
    ///   reordered, or wrapping non-verbatim text.
    /// - [`SynthesisError::CostBudgetExceeded`] — `llm_call_count` >= budget.
    /// - [`SynthesisError::Llm`] — LLM transport failure (not retried).
    pub async fn stage5_compose(
        &mut self,
        _synthesis_id: Uuid,
        query: &str,
        clusters: &[Cluster],
    ) -> Result<String, SynthesisError> {
        let section = self
            .skill
            .section(episcience_core::synthesis::skill::SynthesisStage::Composition)
            .unwrap_or("");
        let prompt = build_compose_prompt(section, query, clusters);
        // Closure captures by reference must outlive the validator call; clone
        // into an owned Vec so the closure is `Fn` without lifetime headaches.
        let clusters_clone = clusters.to_vec();
        let response = self
            .call_llm_with_retry(&prompt, 1, |json| {
                let narrative = json.get("narrative").and_then(|v| v.as_str()).unwrap_or("");
                for c in &clusters_clone {
                    let begin = format!("<<<CLUSTER:{}:BEGIN>>>", c.id);
                    let end = format!("<<<CLUSTER:{}:END>>>", c.id);
                    let extracted = match (narrative.find(&begin), narrative.find(&end)) {
                        (Some(b), Some(e)) if b < e => &narrative[b + begin.len()..e],
                        _ => {
                            return Err(SynthesisError::ComposeAnchorViolation {
                                cluster_id: c.id,
                            });
                        }
                    };
                    if extracted != c.summary {
                        return Err(SynthesisError::ComposeAnchorViolation { cluster_id: c.id });
                    }
                }
                Ok(())
            })
            .await?;
        let mut narrative = response["narrative"].as_str().unwrap_or("").to_string();
        // Strip anchors post-validation so callers receive clean Markdown.
        for c in clusters {
            narrative = narrative.replace(&format!("<<<CLUSTER:{}:BEGIN>>>", c.id), "");
            narrative = narrative.replace(&format!("<<<CLUSTER:{}:END>>>", c.id), "");
        }
        Ok(narrative)
    }
}

// Stage 6 — Verify. Needs no `LlmClient` / `EdgeProvider` bounds — it only
// delegates to `self.skill.verify`, which is part of the unbounded
// `SynthesisSkill` trait. Keeping it in its own unbounded `impl` block
// matches Stages 1 and 3's pattern.
impl<L, P> SynthesisPipeline<L, P> {
    /// Stage 6 — Verify.
    ///
    /// Runs the active skill's verifier against the composed narrative.
    /// Returns the outcome so the caller can route Accept → publish,
    /// Reject → refine (Phase 7) or rejected.
    ///
    /// The default skill verifier (`default_citation_rubric`) enforces:
    /// every cluster member is cited, no citation refers outside the
    /// cluster. Skill-specific overrides can add stricter checks.
    pub async fn stage6_verify(
        &self,
        synthesis_id: Uuid,
        query: &str,
        narrative: &str,
        cluster_member_ids: &[Uuid],
    ) -> Result<episcience_core::synthesis::verifier::VerificationOutcome, SynthesisError> {
        let ctx = episcience_core::synthesis::verifier::VerificationContext {
            synthesis_id,
            query,
            narrative,
            cluster_member_ids,
        };
        Ok(self.skill.verify(&ctx).await)
    }

    /// Stage 7 — Novelty.
    ///
    /// Scores the accepted narrative against prior syntheses using the
    /// supplied backend. Score is persisted on the row in `novelty_score`
    /// (JSONB) by the job handler. Failures here are non-fatal at the
    /// handler level — novelty is metadata, not gating — but this method
    /// surfaces them as [`SynthesisError::Db`] so callers can log and
    /// continue.
    pub async fn stage7_novelty(
        &self,
        synthesis_id: Uuid,
        narrative: &str,
        cluster_member_ids: &[Uuid],
        backend: &dyn episcience_core::synthesis::novelty::NoveltyBackend,
    ) -> Result<episcience_core::synthesis::novelty::NoveltyScore, SynthesisError> {
        backend
            .score(synthesis_id, narrative, cluster_member_ids)
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))
    }
}

/// Build the Stage 5 compose prompt.
///
/// Embeds each cluster's summary inside its sentinel block in the prompt
/// itself, so the LLM has a literal template to copy through. The validator
/// then re-extracts and compares byte-for-byte against `cluster.summary`.
fn build_compose_prompt(skill_section: &str, query: &str, clusters: &[Cluster]) -> String {
    let cluster_blocks: String = clusters
        .iter()
        .map(|c| {
            format!(
                "Cluster {} (title: {}):\n<<<CLUSTER:{}:BEGIN>>>{}<<<CLUSTER:{}:END>>>\n",
                c.cluster_index, c.title, c.id, c.summary, c.id
            )
        })
        .collect();
    // The query must appear in the intro sentence (not appended after the
    // skill guidance) — otherwise the non-empty-section branch reads as
    // "Skill guidance: <text>: <query>." which attaches the query to the
    // skill section instead of to the intro.
    let intro = if skill_section.is_empty() {
        format!("Compose a Markdown narrative answering the query: {query}.")
    } else {
        format!(
            "Compose a Markdown narrative answering the query: {query}.\n\n\
             Skill guidance: {skill_section}"
        )
    };
    format!(
        "{intro}\n\n\
         You are given the following per-cluster summaries. You MUST embed each cluster's \
         summary VERBATIM (byte-for-byte, including the surrounding sentinels) inside the narrative. \
         Do not modify, paraphrase, or rearrange the bracketed claim citations inside.\n\n\
         {cluster_blocks}\n\n\
         Return strict JSON: {{\"narrative\": \"<full markdown text including the sentinel blocks unchanged>\"}}.\n\
         CRITICAL: every <<<CLUSTER:{{id}}:BEGIN>>> ... <<<CLUSTER:{{id}}:END>>> block must appear EXACTLY as given.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use epigraph_cli::enrichment::llm_client::{LlmClient, LlmError};
    use epigraph_embeddings::errors::EmbeddingError;
    use epigraph_embeddings::service::{EmbeddingService, SimilarClaim, TokenUsage};
    use episcience_core::synthesis::skills::default_skill;
    use episcience_core::synthesis::traversal::{EdgeProvider, EdgeType};
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    // ── Mock dependencies ────────────────────────────────────────────────
    //
    // None of these touch the DB. The pool itself is constructed with
    // `connect_lazy` so the test runs without a server.

    #[derive(Debug, Default)]
    struct MockLlm;

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn complete_json(&self, _prompt: &str) -> Result<serde_json::Value, LlmError> {
            Ok(serde_json::json!({}))
        }
        fn model_name(&self) -> &str {
            "mock"
        }
    }

    #[derive(Debug, Default)]
    struct MockEdge;

    #[async_trait]
    impl EdgeProvider for MockEdge {
        async fn neighbors(&self, _claim: Uuid, _types: &[EdgeType]) -> Vec<(Uuid, EdgeType)> {
            vec![]
        }
    }

    /// Stub embedder: every method returns an `Err` or an empty result.
    /// The skill-field tests never invoke embedding; this exists only so
    /// `SynthesisPipeline::new` can be constructed.
    #[derive(Debug, Default)]
    struct StubEmbedder;

    #[async_trait]
    impl EmbeddingService for StubEmbedder {
        async fn generate(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
            Err(EmbeddingError::ApiError {
                message: "stub: generate disabled".into(),
                status_code: None,
            })
        }

        async fn batch_generate(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Err(EmbeddingError::ApiError {
                message: "stub: batch_generate disabled".into(),
                status_code: None,
            })
        }

        async fn store(&self, _claim_id: Uuid, _embedding: &[f32]) -> Result<(), EmbeddingError> {
            Err(EmbeddingError::ApiError {
                message: "stub: store disabled".into(),
                status_code: None,
            })
        }

        async fn get(&self, claim_id: Uuid) -> Result<Vec<f32>, EmbeddingError> {
            Err(EmbeddingError::NotFound { claim_id })
        }

        async fn similar(
            &self,
            _embedding: &[f32],
            _k: usize,
            _min_similarity: f32,
        ) -> Result<Vec<SimilarClaim>, EmbeddingError> {
            Err(EmbeddingError::ApiError {
                message: "stub: similar disabled".into(),
                status_code: None,
            })
        }

        fn dimension(&self) -> usize {
            1536
        }

        fn token_usage(&self) -> TokenUsage {
            TokenUsage::default()
        }

        fn reset_token_usage(&self) {}

        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Err(EmbeddingError::ApiError {
                message: "stub: health_check disabled".into(),
                status_code: None,
            })
        }
    }

    fn lazy_pool() -> sqlx::PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://test:test@127.0.0.1:5432/test")
            .expect("lazy pool must construct without a DB roundtrip")
    }

    fn build_test_pipeline() -> SynthesisPipeline<MockLlm, MockEdge> {
        SynthesisPipeline::new(
            lazy_pool(),
            Arc::new(StubEmbedder),
            MockLlm,
            MockEdge,
            vec![],
            20,
        )
    }

    // `#[tokio::test]` rather than `#[test]`: `PgPoolOptions::connect_lazy`
    // spawns a background reaper task during construction, which panics
    // outside a Tokio runtime. The tests still perform no DB roundtrips;
    // the runtime is only needed for pool construction.

    #[tokio::test]
    async fn pipeline_default_skill_is_baseline() {
        let pipeline = build_test_pipeline();
        assert_eq!(pipeline.skill.name(), default_skill().name());
        assert_eq!(pipeline.skill.name(), "baseline");
    }

    /// Distinct skill used to prove that `with_skill` actually mutates the
    /// field (not just that the builder type-checks). When Phase 5 ships
    /// `LabNotebookSkill`, swap to that instead so removing this stub
    /// becomes a compile error instead of a silent test gap.
    #[derive(Debug)]
    struct AltSkill;

    #[async_trait]
    impl episcience_core::synthesis::skill::SynthesisSkill for AltSkill {
        fn name(&self) -> &'static str {
            "alt"
        }
        fn section(
            &self,
            _stage: episcience_core::synthesis::skill::SynthesisStage,
        ) -> Option<&str> {
            None
        }
    }

    #[tokio::test]
    async fn with_skill_replaces_the_default_skill() {
        let pipeline = build_test_pipeline().with_skill(Arc::new(AltSkill));
        assert_eq!(pipeline.skill.name(), "alt");
    }

    /// Build the smallest viable `Cluster` for prompt-building tests. The
    /// cluster has a single synthetic member id; the narrate-prompt builder
    /// only reads `id`, `cluster_index`, and `member_claim_ids`.
    fn minimal_cluster() -> Cluster {
        Cluster {
            id: Uuid::new_v4(),
            synthesis_id: Uuid::new_v4(),
            cluster_index: 0,
            title: String::new(),
            summary: String::new(),
            member_claim_ids: vec![Uuid::new_v4()],
            support_count: 0,
            contradict_count: 0,
        }
    }

    /// Non-empty `skill_section` is injected verbatim into the prompt under
    /// the "Skill guidance:" prefix. The literal sentinel exercises the full
    /// substitution path without depending on `BaselineSkill`'s exact text.
    #[test]
    fn build_narrate_prompt_includes_skill_section() {
        let cluster = minimal_cluster();
        let prompt = build_narrate_prompt(
            "INJECTED-SECTION-MARKER-12345",
            &cluster,
            &[],
            &serde_json::json!({}),
        );
        assert!(
            prompt.contains("INJECTED-SECTION-MARKER-12345"),
            "expected skill section to be injected into prompt, got: {prompt}"
        );
        assert!(
            prompt.contains("Skill guidance:"),
            "expected 'Skill guidance:' prefix when section is non-empty"
        );
    }

    /// Empty `skill_section` preserves the pre-skill prompt verbatim — no
    /// "Skill guidance:" line, intro identical to the original.
    #[test]
    fn build_narrate_prompt_empty_section_preserves_baseline() {
        let cluster = minimal_cluster();
        let prompt = build_narrate_prompt("", &cluster, &[], &serde_json::json!({}));
        assert!(
            prompt.starts_with("You are summarizing a cluster of related claims"),
            "expected prompt to begin with original intro, got: {prompt}"
        );
        assert!(
            !prompt.contains("Skill guidance:"),
            "expected no 'Skill guidance:' line when section is empty, got: {prompt}"
        );
    }

    /// Non-empty `skill_section` is injected verbatim into the compose prompt
    /// under the "Skill guidance:" prefix. Mirror of the narrate test, using
    /// a distinct sentinel so failures point at the correct prompt builder.
    #[test]
    fn build_compose_prompt_includes_skill_section() {
        let cluster = minimal_cluster();
        let prompt =
            build_compose_prompt("INJECTED-SECTION-MARKER-67890", "test query", &[cluster]);
        assert!(
            prompt.contains("INJECTED-SECTION-MARKER-67890"),
            "expected skill section to be injected into prompt, got: {prompt}"
        );
        assert!(
            prompt.contains("Skill guidance:"),
            "expected 'Skill guidance:' prefix when section is non-empty"
        );
    }

    /// Stage 6 verifier wired through the pipeline: a narrative that omits
    /// a cluster member should land in `Reject{UncitedMember}`. Proves the
    /// pipeline's delegation to `self.skill.verify` works end-to-end and
    /// `stage6_verify` returns the outcome verbatim.
    #[tokio::test]
    async fn stage6_verify_returns_reject_for_uncited_member() {
        use episcience_core::synthesis::verifier::{VerificationOutcome, VerificationReason};
        let pipeline = build_test_pipeline();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let narrative = format!("Only mentions [{a}], not the other.");
        let outcome = pipeline
            .stage6_verify(Uuid::new_v4(), "test query", &narrative, &[a, b])
            .await
            .expect("verify should not error");
        match outcome {
            VerificationOutcome::Reject {
                reason: VerificationReason::UncitedMember { claim_id },
                ..
            } => {
                assert_eq!(claim_id, b, "expected reject pointing at uncited member b");
            }
            other => panic!("expected Reject{{UncitedMember}}, got {other:?}"),
        }
    }

    /// Empty `skill_section` preserves the original compose prompt byte-for-
    /// byte: no "Skill guidance:" line and the colon-splice keeps the query
    /// inline as `": test query."` immediately after the intro sentence.
    #[test]
    fn build_compose_prompt_empty_section_preserves_baseline() {
        let prompt = build_compose_prompt("", "test query", &[]);
        assert!(
            prompt.starts_with("Compose a Markdown narrative answering the query"),
            "expected prompt to begin with original intro, got: {prompt}"
        );
        assert!(
            !prompt.contains("Skill guidance:"),
            "expected no 'Skill guidance:' line when section is empty, got: {prompt}"
        );
        assert!(
            prompt.contains(": test query."),
            "expected colon-splice to keep query inline as ': test query.', got: {prompt}"
        );
    }
}
