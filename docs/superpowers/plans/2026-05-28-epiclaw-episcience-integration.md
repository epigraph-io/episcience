# EpiClaw ↔ Episcience Integration Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn EpiClaw's autonomous workflow runs (arxiv research scan, nightly bug-fix pipeline, weekly capability audit, etc.) into verifier-accepted, novelty-scored, citation-disciplined narratives in episcience — and use episcience's MCP write tools to make every task artifact (observations, blobs, countersignatures) a first-class graph entity rather than opaque provenance telemetry.

**Architecture:** Three integration shapes, layered:
1. **Sample-as-workflow-run.** Every successful EpiClaw workflow run inserts a `samples` row with `sample_type='workflow_run'`; subsequent claims emitted during the run are tied to that sample via `sample_claims`. The synthesis pipeline can then seed off the sample and produce a narrative summary of the run.
2. **Per-workflow `SynthesisSkill`.** Three new skill specialisations (`literature`, `code_review`, `registry_diff`) cover the three major workflow shapes EpiClaw runs. Each overrides Narration / Composition / Verifier so the synthesis output matches the workflow's domain (literature citations, PR review formatting, capability registry diffs).
3. **EpiClaw → episcience MCP fan-out.** EpiClaw's `ProvenanceRecorder` and scheduler gain hooks that emit `add_observation`, `attach_blob`, `synthesize`, and `countersign` calls via the existing episcience MCP server. Workflows opt in via `workflow.properties.synthesis_skill` (a one-line addition to the stored workflow).

**Tech Stack:** Rust (axum 0.7, sqlx 0.7, tokio, async-trait 0.1, rmcp), PostgreSQL 16, BLAKE3, Ed25519, MCP. Episcience repo at `/home/jeremy/episcience`. EpiClaw repo at `/home/jeremy/epiclaw-host`.

**Repos and dev DBs:**
- Episcience: `postgres://epigraph:epigraph@127.0.0.1:5432/epigraph` (dev), `epigraph_db_repo_test` (test).
- EpiClaw uses its own SQLite scheduler DB at `{data_dir}/scheduler.db`; emits claims to the shared EpiGraph kernel at `http://127.0.0.1:8090`.

**Reference:** the prior SciLink-lessons plan at `/home/jeremy/episcience/docs/superpowers/plans/2026-05-27-scilink-lessons-into-episcience.md` documents the foundation (skill trait, BaselineSkill + LabNotebookSkill, verifier, novelty, refinement, MCP writes, protocol sections) all merged in PRs #4–#9 + #11. This plan builds on that foundation; do not re-introduce concepts it already establishes.

---

## Phase / repo / shipping summary

| Phase | Owner | What ships | Stand-alone PR? |
|---|---|---|---|
| 0 | n/a | Branch + baseline tests (per repo) | scaffolding |
| 1 | **episcience** | `sample_type='workflow_run'` allowed; helper to insert + link | ✅ |
| 2 | **episcience** | `LiteratureSkill` (registered + tested) | ✅ |
| 3 | **episcience** | `CodeReviewSkill` (registered + tested) | ✅ |
| 4 | **episcience** | `RegistryDiffSkill` (registered + tested) | ✅ |
| 5 | **epiclaw** | Post-workflow synthesis hook (calls episcience MCP) | ✅ (depends on 1+2) |
| 6 | **epiclaw** | `add_observation` for task outputs (alongside provenance claim) | ✅ |
| 7 | **epiclaw** | `attach_blob` for `/workspace/group/` artifacts | ✅ |
| 8 | **shared** | Countersign-as-merge-gate for nightly-bug-fix | ✅ (depends on 3 + 5) |
| 9 | **episcience** | `PaperNoveltyBackend` (scores ingest candidates) | ✅ |
| 10 | **shared** | ProtocolSections-aligned schedules.toml parsing | ✅ |
| 11 | both | Docs + onboarding update + finishing | wrap-up |

Each phase produces independently testable, mergeable code. Recommended ship order: 1 → 2 → 5 (smallest end-to-end demo: a workflow run produces a verifier-accepted narrative). Then 3 / 4 / 6 / 7 / 9 / 10 in any order. Phase 8 stacks on 3 + 5.

---

## Scope of `workflow_id`

EpiGraph workflows already have a `workflow_id UUID` and a `canonical_name TEXT` plus an integer `generation`. EpiClaw's scheduled tasks can carry `workflow_id` via the existing `ScheduledTask` schema. This plan does NOT change EpiGraph's workflow schema. It DOES:
- Add `sample_type='workflow_run'` as an allowed value on the episcience `samples` table (migration extends the existing check constraint).
- Treat the EpiClaw `workflow_id` as the value of `samples.properties.workflow_id` (JSONB field) for cross-repo linkage. No FK across the two databases; the workflow_id is a content-addressed string identifier on both sides.
- Inject `workflow_id` into the episcience MCP `synthesize` request's `query` field as a structured prefix (`"workflow_run:<uuid>: ..."`) so the synthesis row's query is queryable by workflow.

---

## File structure (post-plan)

### New files

```
# Episcience
crates/episcience-core/src/synthesis/skills/literature.rs               — LiteratureSkill
crates/episcience-core/src/synthesis/skills/code_review.rs              — CodeReviewSkill
crates/episcience-core/src/synthesis/skills/registry_diff.rs            — RegistryDiffSkill
crates/episcience-core/src/synthesis/skills/markdown/literature.md      — skill reference
crates/episcience-core/src/synthesis/skills/markdown/code_review.md     — skill reference
crates/episcience-core/src/synthesis/skills/markdown/registry_diff.md   — skill reference
crates/episcience-db/src/synthesis/novelty_backend_paper.rs             — PaperNoveltyBackend (Phase 9)
crates/episcience-api/src/routes/workflow_runs.rs                       — POST /api/v1/eln/workflow_runs (Phase 1)
migrations/5026_samples_workflow_run.sql                                — sample_type CHECK extension (Phase 1)
migrations/synthesis/5028a_syntheses_skill_literature.sql               — CHECK extension for literature skill (Phase 2)
migrations/synthesis/5028b_syntheses_skill_code_review.sql              — CHECK extension for code_review skill (Phase 3)
migrations/synthesis/5028c_syntheses_skill_registry_diff.sql            — CHECK extension for registry_diff skill (Phase 4)

# EpiClaw
src/host/episcience_client.rs                                           — MCP/HTTP client (Phase 5)
src/host/workflow_run_hook.rs                                           — post-task synthesis trigger (Phase 5)
src/host/observations.rs                                                — add_observation wrapper (Phase 6)
src/host/blob_uploader.rs                                               — attach_blob wrapper (Phase 7)
docs/integration-with-episcience.md                                     — operator guide (Phase 11)
```

### Modified files

```
# Episcience
crates/episcience-core/src/synthesis/skills/mod.rs                      — registry: 3 new arms
crates/episcience-core/src/sample.rs                                    — SampleType variant 'workflow_run'
crates/episcience-db/src/repos/sample.rs                                — accept new sample_type
crates/episcience-api/src/routes/mod.rs                                 — mount workflow_runs router
crates/episcience-api/src/mcp/synthesize.rs                             — accept skill_name + workflow_id
crates/episcience-api/src/jobs/synthesis_job.rs                         — Phase 9 hook for paper-novelty backend

# EpiClaw
src/host/provenance.rs                                                  — call observations.rs in record_task_executed
src/host/scheduler.rs                                                   — wire workflow_run_hook after each task
src/host/container.rs                                                   — blob upload on container exit (Phase 7)
src/host/config.rs                                                      — EPISCIENCE_URL env var
Cargo.toml                                                              — new crate deps if any
README.md                                                               — link the new docs

# Both
README.md                                                               — add an integration banner pointing at docs/
```

---

## Phase 0 — Per-repo branch + baseline

This work spans two repos. Each gets its own worktree.

### Task 0.1 — Episcience baseline

- [ ] **Step 1: Pull main + verify clean baseline**

```bash
cd /home/jeremy/episcience && git checkout main && git pull origin main
git rev-parse HEAD  # should match the latest merged PR head
```

- [ ] **Step 2: Create the integration worktree**

```bash
cd /home/jeremy/episcience && git worktree add /home/jeremy/episcience-wt-integration -b feat/epiclaw-integration-phase1 origin/main
```

- [ ] **Step 3: Confirm baseline tests pass**

```bash
cd /home/jeremy/episcience-wt-integration && \
  DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
  cargo test --workspace --lib --bins 2>&1 | grep "test result"
```

Expected: 5 "test result: ok" lines summing to the post-merge count (≈ 73 if all SciLink phases landed).

- [ ] **Step 4: Empty commit to mark start**

```bash
cd /home/jeremy/episcience-wt-integration && git commit --allow-empty -m "chore: start EpiClaw integration Phase 1"
```

### Task 0.2 — EpiClaw baseline

- [ ] **Step 1: Verify worktree origin**

```bash
cd /home/jeremy/epiclaw-host && git remote -v
# Expected: origin -> github.com/tylorsama/epiclaw-host (per project_epiclaw_host_repo memory)
git pull origin main
```

- [ ] **Step 2: Create the EpiClaw integration worktree**

```bash
cd /home/jeremy/epiclaw-host && git worktree add /home/jeremy/epiclaw-host-wt-integration -b feat/episcience-integration-phase5 origin/main
```

(Phase 5 is the first EpiClaw-side change. The Phase 6/7/8/10 EpiClaw work can branch from Phase 5's branch or rebase onto main once Phase 5 merges.)

- [ ] **Step 3: Confirm EpiClaw baseline builds + tests pass**

```bash
cd /home/jeremy/epiclaw-host-wt-integration && cargo test --workspace 2>&1 | grep "test result"
```

Expected: all green (record the count for regression checks).

- [ ] **Step 4: Empty commit**

```bash
cd /home/jeremy/epiclaw-host-wt-integration && git commit --allow-empty -m "chore: start episcience integration Phase 5"
```

---

## Phase 1 — Episcience: `sample_type='workflow_run'` + workflow-run helper

Goal: let episcience accept and store EpiClaw workflow-run records as `samples` rows. Add a thin HTTP route `POST /api/v1/eln/workflow_runs` that takes a `workflow_id` + `canonical_name` + `started_at` and atomically inserts the sample + a `properties.workflow_id` JSON field.

This phase introduces ZERO new concepts. It just opens the door for Phase 5 (EpiClaw) to start posting workflow-run rows.

### Task 1.1 — Migration: extend `samples_sample_type_check` to include `workflow_run`

**Files:**
- Create: `migrations/5026_samples_workflow_run.sql`

- [ ] **Step 1: Write the migration**

```sql
-- 5026_samples_workflow_run.sql
-- Extend the samples_sample_type_check CHECK constraint to allow
-- 'workflow_run' alongside the existing sample types. EpiClaw posts one
-- workflow_run sample per successful task; downstream syntheses cite it
-- via sample_claims.
ALTER TABLE samples
    DROP CONSTRAINT IF EXISTS samples_sample_type_check;
ALTER TABLE samples
    ADD CONSTRAINT samples_sample_type_check
    CHECK (sample_type IN (
        -- pre-existing types; preserve order
        'dna_origami', 'protein_construct', 'substrate', 'reagent',
        'aliquot', 'dataset_file',
        -- new in Phase 1 of EpiClaw integration
        'workflow_run'
    ));
```

> Note: the pre-existing CHECK list above is inferred from `migrations/5003_create_samples.sql` and `docs/intro/02-concepts-science.md`. **Verify the actual values before writing the migration:** `psql ... -c "\d samples" | grep samples_sample_type_check`. Use the actual list in your CHECK — don't trust the snippet above blindly.

- [ ] **Step 2: Apply to test DB only**

```bash
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -f /home/jeremy/episcience-wt-integration/migrations/5026_samples_workflow_run.sql
```

Expected: 2 ALTER TABLE lines, no errors.

- [ ] **Step 3: Verify**

```bash
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -c "\d samples" | grep samples_sample_type_check
```

Expected: includes `'workflow_run'`.

- [ ] **Step 4: Commit**

```bash
git add migrations/5026_samples_workflow_run.sql
git commit -m "feat(db): allow sample_type='workflow_run' (Phase 1)

EpiClaw posts one workflow_run sample per successful task; downstream
syntheses cite it via sample_claims. Pre-existing sample types are
preserved."
```

### Task 1.2 — Extend `SampleType` enum in core

**Files:**
- Modify: `crates/episcience-core/src/sample.rs`

- [ ] **Step 1: Find the existing enum**

```bash
grep -n "pub enum SampleType\|impl FromStr for SampleType\|fn as_str.*SampleType" crates/episcience-core/src/sample.rs | head -5
```

- [ ] **Step 2: Add the `WorkflowRun` variant**

Find the `pub enum SampleType { ... }` block. Add `WorkflowRun` at the end:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleType {
    DnaOrigami,
    ProteinConstruct,
    Substrate,
    Reagent,
    Aliquot,
    DatasetFile,
    /// EpiClaw workflow run. The sample's `name` field is the workflow's
    /// canonical_name; `properties.workflow_id` carries the EpiGraph
    /// workflow UUID for cross-system linkage.
    WorkflowRun,
}
```

Update `impl FromStr for SampleType` and any `as_str` to handle the new variant. **Verify the exact existing surface before editing** — the pre-existing variants may have different names than the snippet above.

- [ ] **Step 3: Write a round-trip test**

In the inline `#[cfg(test)] mod tests { ... }` block at the bottom of `sample.rs`:

```rust
#[test]
fn workflow_run_sample_type_round_trips() {
    let s = SampleType::WorkflowRun;
    let serialized = s.as_str();
    assert_eq!(serialized, "workflow_run");
    let parsed: SampleType = serialized.parse().expect("workflow_run parses");
    assert_eq!(parsed, SampleType::WorkflowRun);
}
```

- [ ] **Step 4: Run + commit**

```bash
cd /home/jeremy/episcience-wt-integration && \
  cargo test -p episcience-core sample::tests::workflow_run_sample_type
# Expect: 1 passed.
git add crates/episcience-core/src/sample.rs
git commit -m "feat(core): SampleType::WorkflowRun variant (Phase 1)"
```

### Task 1.3 — HTTP route `POST /api/v1/eln/workflow_runs`

**Files:**
- Create: `crates/episcience-api/src/routes/workflow_runs.rs`
- Modify: `crates/episcience-api/src/routes/mod.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/episcience-api/tests/workflow_runs_route_test.rs` (new file). Use the existing `synthesis_job_handler_test.rs` test-server pattern as the template — copy `build_test_server`, `mint_test_jwt`, `bearer`, `connect`, and `test_agent_id` helpers verbatim from one of the existing route tests (the duplication is a known tech debt; fixing it is out of scope here).

```rust
#[tokio::test]
async fn post_workflow_run_creates_sample_with_workflow_id_property() {
    let app = build_test_server().await;
    let auth_agent = test_agent_id();
    let workflow_id = Uuid::new_v4();
    let body = serde_json::json!({
        "workflow_id": workflow_id,
        "canonical_name": "research-scan-ingest-morning",
        "prepared_by": auth_agent,
        "started_at": "2026-05-28T03:00:00Z",
    });
    let resp = app.post("/api/v1/eln/workflow_runs")
        .add_header("Authorization", bearer(&mint_test_jwt(auth_agent)).parse().unwrap(), )
        .json(&body)
        .await;
    resp.assert_status(StatusCode::CREATED);
    let sample_id: Uuid = resp.json::<serde_json::Value>()["sample_id"]
        .as_str().unwrap().parse().unwrap();

    // Verify the sample row carries the workflow_id under properties.
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT properties FROM samples WHERE id = $1"
    )
    .bind(sample_id)
    .fetch_one(&app.pool)
    .await.unwrap();
    assert_eq!(stored["workflow_id"], workflow_id.to_string());

    let sample_type: String = sqlx::query_scalar(
        "SELECT sample_type FROM samples WHERE id = $1"
    )
    .bind(sample_id)
    .fetch_one(&app.pool)
    .await.unwrap();
    assert_eq!(sample_type, "workflow_run");
}
```

- [ ] **Step 2: Run to confirm it fails**

```bash
cd /home/jeremy/episcience-wt-integration && \
  DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
  cargo test -p episcience-api --test workflow_runs_route_test
```

Expected: compile FAIL or 404 at runtime — the route doesn't exist yet.

- [ ] **Step 3: Implement the route**

Create `crates/episcience-api/src/routes/workflow_runs.rs`:

```rust
//! `POST /api/v1/eln/workflow_runs` — register an EpiClaw workflow run as
//! a `samples` row with `sample_type='workflow_run'`. The workflow's
//! UUID + canonical_name carry through to `samples.properties` so the
//! row stays cross-referenceable with EpiGraph's workflows table.

use axum::{
    extract::{Extension, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::errors::ApiError;
use crate::middleware::AuthContext;
use crate::state::ElnState;

#[derive(Debug, Deserialize)]
pub struct CreateWorkflowRunRequest {
    pub workflow_id: Uuid,
    pub canonical_name: String,
    pub prepared_by: Uuid,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub labels: Vec<String>,
}

pub fn router() -> Router<ElnState> {
    Router::new().route("/api/v1/eln/workflow_runs", post(create_workflow_run))
}

async fn create_workflow_run(
    State(state): State<ElnState>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<CreateWorkflowRunRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if auth.agent_id != req.prepared_by {
        return Err(ApiError::Forbidden(
            "prepared_by must match the authenticated agent".into(),
        ));
    }

    let id = Uuid::now_v7();
    let mut labels = req.labels;
    labels.push("workflow_run".to_string());
    let properties = serde_json::json!({
        "workflow_id": req.workflow_id.to_string(),
        "canonical_name": req.canonical_name,
        "started_at": req.started_at.to_rfc3339(),
    });
    // content_hash = BLAKE3 over canonical_name + workflow_id + started_at
    let mut hasher = blake3::Hasher::new();
    hasher.update(req.canonical_name.as_bytes());
    hasher.update(req.workflow_id.as_bytes());
    hasher.update(req.started_at.to_rfc3339().as_bytes());
    let content_hash = hasher.finalize();

    sqlx::query(
        "INSERT INTO samples
            (id, name, sample_type, status, prepared_by, preparation_date,
             quantity_value, quantity_unit, hazard_info, labels, properties,
             content_hash)
         VALUES ($1, $2, 'workflow_run', 'prepared', $3, $4,
                 1.0, 'run', '{}'::jsonb, $5, $6, $7)",
    )
    .bind(id)
    .bind(&req.canonical_name)
    .bind(req.prepared_by)
    .bind(req.started_at)
    .bind(&labels)
    .bind(&properties)
    .bind(content_hash.as_bytes())
    .execute(&state.pool)
    .await?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "sample_id": id,
            "sample_type": "workflow_run",
            "workflow_id": req.workflow_id,
        })),
    ))
}
```

Mount the router in `crates/episcience-api/src/routes/mod.rs` (add `pub mod workflow_runs;` and merge `workflow_runs::router()` into the top-level router builder — find the existing `Router::new().merge(syntheses::router())` style and follow it).

- [ ] **Step 4: Re-run the test**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
  cargo test -p episcience-api --test workflow_runs_route_test
```

Expected: 1 passed.

- [ ] **Step 5: Run the full integration suite to catch route-table regressions**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
  cargo test -p episcience-api --tests
```

Expected: existing tests still pass; new test joins them.

- [ ] **Step 6: Commit**

```bash
git add crates/episcience-api/src/routes/workflow_runs.rs \
        crates/episcience-api/src/routes/mod.rs \
        crates/episcience-api/tests/workflow_runs_route_test.rs
git commit -m "feat(api): POST /eln/workflow_runs creates a workflow_run sample

EpiClaw posts here once per scheduled-task success. The sample row
carries workflow_id + canonical_name + started_at under properties.
Subsequent observations / blobs / countersignatures can attach via
sample_id; the synthesis pipeline can cluster off this sample to
produce a narrative summary of the run."
```

### Task 1.4 — Push Phase 1 + create PR

- [ ] **Step 1: Push**

```bash
git push -u origin feat/epiclaw-integration-phase1
```

- [ ] **Step 2: Create the PR**

```bash
gh pr create --base main --title "feat(eln): workflow_run sample type + POST /eln/workflow_runs (Phase 1)" --body "$(cat <<'EOF'
## Summary

Phase 1 of the EpiClaw ↔ episcience integration plan. Adds:
- Migration 5026: extend samples_sample_type_check to allow 'workflow_run'.
- SampleType::WorkflowRun variant in episcience-core.
- POST /api/v1/eln/workflow_runs route that creates a workflow_run sample.

No EpiClaw-side changes. Phase 5 will start using this route.

## Test plan

- [x] Migration applied to test DB; constraint extended.
- [x] cargo test passes for the new route + existing tests.
- [x] Dev DB untouched.
EOF
)"
```

---

## Phase 2 — Episcience: `LiteratureSkill`

Goal: the first non-baseline, non-lab_notebook skill from this plan. Tailored for the arxiv research scan workflow.

### Task 2.1 — Migration: extend `syntheses_skill_name_known` to include `literature`

**Files:**
- Create: `migrations/synthesis/5028a_syntheses_skill_literature.sql`

Same pattern as Phase 5 (LabNotebookSkill) in the prior plan. Drop + re-add the constraint with the new skill in the IN list:

```sql
ALTER TABLE syntheses
    DROP CONSTRAINT syntheses_skill_name_known;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline', 'lab_notebook', 'literature'));
```

Apply to test DB only.

### Task 2.2 — `LiteratureSkill` impl

**Files:**
- Create: `crates/episcience-core/src/synthesis/skills/literature.rs`
- Create: `crates/episcience-core/src/synthesis/skills/markdown/literature.md`
- Modify: `crates/episcience-core/src/synthesis/skills/mod.rs`

- [ ] **Step 1: Write the failing tests**

`crates/episcience-core/src/synthesis/skills/literature.rs`:

```rust
//! `LiteratureSkill` — synthesis tuned for the arxiv research scan
//! workflow.
//!
//! Differs from baseline in three ways:
//! - traversal narrows to `Supports + Methodology + Corroborates` edges
//!   (the citation-discipline trio for literature work)
//! - narration explicitly demands DOI / arxiv citation formatting
//! - verifier inherits the default citation rubric (every cluster member
//!   cited) — no override

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};

#[derive(Debug, Default)]
pub struct LiteratureSkill;

#[async_trait::async_trait]
impl SynthesisSkill for LiteratureSkill {
    fn name(&self) -> &'static str { "literature" }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview =>
                "Summarise a literature-scan run: which papers were found, \
                 which were already known, which contributed novel findings.",
            SynthesisStage::Narration =>
                "For each cluster, list the papers it covers. Cite each \
                 with `[<claim_id>]` and ALSO with the paper's DOI in \
                 parentheses: `(doi:10.xxx/yyy)`. If a paper has no DOI, \
                 use `(arxiv:NNNN.NNNNN)`. Group by methodology or topic. \
                 Do not invent identifiers.",
            SynthesisStage::Composition =>
                "Compose the per-cluster summaries into one Markdown \
                 narrative, ordered by methodology family then publication \
                 date. Keep the `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels \
                 verbatim.",
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        Some(TraversalConfig {
            max_hops: 3,
            edge_types: vec![
                EdgeType::Supports,
                EdgeType::Methodology,
                EdgeType::Corroborates,
            ],
            relevance_prune: 0.5,
            ..TraversalConfig::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literature_skill_overrides_three_stages() {
        let s = LiteratureSkill;
        assert_eq!(s.name(), "literature");
        let narration = s.section(SynthesisStage::Narration).unwrap();
        assert!(narration.contains("DOI"));
        assert!(narration.contains("arxiv"));
        let composition = s.section(SynthesisStage::Composition).unwrap();
        assert!(composition.to_lowercase().contains("methodology"));
        // Verification falls back to the default rubric — no override.
        assert!(s.section(SynthesisStage::Verification).is_none());
        // Traversal is opinionated.
        let cfg = s.traversal_config().unwrap();
        assert_eq!(cfg.max_hops, 3);
        assert_eq!(cfg.edge_types.len(), 3);
    }
}
```

- [ ] **Step 2: Register in mod.rs**

In `crates/episcience-core/src/synthesis/skills/mod.rs`, add `pub mod literature;` and extend `load_by_name`:

```rust
"literature" => Some(Arc::new(literature::LiteratureSkill)),
```

- [ ] **Step 3: Markdown reference**

Create `crates/episcience-core/src/synthesis/skills/markdown/literature.md`:

```markdown
---
name: literature
description: Synthesis tuned for arxiv research-scan workflows — DOI
  and arxiv citation formatting, methodology-grouped composition,
  wider traversal across Supports/Methodology/Corroborates edges.
---

# Overview

Summarise a literature-scan run: which papers were found, which were
already known, which contributed novel findings.

# Narration

For each cluster, list the papers it covers. Cite each with
`[<claim_id>]` and ALSO with the paper's DOI in parentheses:
`(doi:10.xxx/yyy)`. If a paper has no DOI, use `(arxiv:NNNN.NNNNN)`.
Group by methodology or topic. Do not invent identifiers.

# Composition

Compose the per-cluster summaries into one Markdown narrative, ordered
by methodology family then publication date. Keep the
`<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.

# Traversal

`max_hops=3`, `relevance_prune=0.5`, edge_types = Supports +
Methodology + Corroborates. Literature work follows wider citation
trails than ELN narratives, so the default depth is loosened.

# Verification

Inherits the default citation rubric: every cluster member must be
cited; no citation may refer outside the cluster.
```

- [ ] **Step 4: Run + commit**

```bash
cd /home/jeremy/episcience-wt-integration && \
  cargo test -p episcience-core synthesis::skills::literature
# Expect: 1 passed.

git add crates/episcience-core/src/synthesis/skills/literature.rs \
        crates/episcience-core/src/synthesis/skills/markdown/literature.md \
        crates/episcience-core/src/synthesis/skills/mod.rs \
        migrations/synthesis/5028a_syntheses_skill_literature.sql
git commit -m "feat(synthesis): LiteratureSkill (Phase 2)

Third concrete SynthesisSkill, tuned for the arxiv research-scan
workflow. Demands DOI / arxiv citation formatting; methodology-grouped
composition; traversal widens to 3 hops across Supports + Methodology
+ Corroborates edge types. Verification inherits the default rubric.

Migration 5028a extends syntheses_skill_name_known to allow 'literature'."
```

### Task 2.3 — Push Phase 2 + create PR

Same shape as Task 1.4. Title: `feat(synthesis): LiteratureSkill (Phase 2)`.

---

## Phase 3 — Episcience: `CodeReviewSkill`

Goal: synthesis specialisation for the nightly-bug-fix pipeline. Outputs a PR-body-shaped narrative.

### Task 3.1 — Migration: extend CHECK list with `code_review`

Same pattern as 2.1. File: `migrations/synthesis/5028b_syntheses_skill_code_review.sql`.

### Task 3.2 — `CodeReviewSkill` impl with stricter verifier

**Files:**
- Create: `crates/episcience-core/src/synthesis/skills/code_review.rs`
- Create: `crates/episcience-core/src/synthesis/skills/markdown/code_review.md`
- Modify: `crates/episcience-core/src/synthesis/skills/mod.rs`

This skill **overrides `verify`** — not just `section`. The default rubric (every cluster member cited) is insufficient for code review; we also need to confirm every PR-number mentioned in the narrative appears as a claim in the cluster.

```rust
//! `CodeReviewSkill` — synthesis tuned for the nightly-bug-fix pipeline.
//!
//! Output is a PR-body-shaped Markdown narrative. Verifier inherits
//! the default citation discipline AND adds a check: every PR number
//! (#NNNN) mentioned in the narrative must correspond to a claim in
//! the cluster carrying a `pr_number` property.

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};
use crate::synthesis::verifier::{
    default_citation_rubric, VerificationContext, VerificationOutcome,
    VerificationReason,
};

#[derive(Debug, Default)]
pub struct CodeReviewSkill;

#[async_trait::async_trait]
impl SynthesisSkill for CodeReviewSkill {
    fn name(&self) -> &'static str { "code_review" }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview =>
                "Summarise a code-change run: which files changed, what \
                 invariants were tested, which PRs opened.",
            SynthesisStage::Narration =>
                "For each cluster, write a PR-body-shaped 3-5 sentence \
                 summary. Cite every claim with `[<claim_id>]`. Cite PRs \
                 as `#<number>` and commits as ``` `<sha>` ``` (7-char \
                 abbreviation acceptable). Do not invent any.",
            SynthesisStage::Composition =>
                "Compose the per-cluster summaries into a Markdown \
                 narrative organised as `## Summary` / `## Files changed` \
                 / `## Test plan` (standard PR shape). Keep the \
                 `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.",
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        Some(TraversalConfig {
            max_hops: 2,
            edge_types: vec![EdgeType::Supports, EdgeType::Methodology],
            relevance_prune: 0.6,
            ..TraversalConfig::default()
        })
    }

    async fn verify(
        &self,
        ctx: &VerificationContext<'_>,
    ) -> VerificationOutcome {
        // Run the default citation rubric first.
        let baseline = default_citation_rubric(ctx);
        if let VerificationOutcome::Reject { .. } = baseline {
            return baseline;
        }
        // Additional check: every #NNNN in the narrative must be a real
        // PR-bearing claim in the cluster. The cluster's claim contents
        // are NOT in the VerificationContext (the verifier only sees
        // the narrative + member ids), so the strictest the skill can
        // check WITHOUT a DB round-trip is: any `#\d{1,5}` pattern is
        // accompanied by a matching `[<uuid>]` citation in the same
        // sentence. The DB-side check belongs in Phase 8 (countersign-
        // as-merge-gate) where the host has access to claim contents.
        let pr_re = regex::Regex::new(r"#(\d{1,6})\b").expect("static");
        for caps in pr_re.captures_iter(ctx.narrative) {
            let pr_num = &caps[1];
            // Search for `[<uuid>]` within ~120 chars of the #NNNN.
            let pos = caps.get(0).unwrap().start();
            let window_start = pos.saturating_sub(120);
            let window_end = (pos + 120).min(ctx.narrative.len());
            let window = &ctx.narrative[window_start..window_end];
            let has_claim = window.contains('[') && window.contains(']');
            if !has_claim {
                return VerificationOutcome::Reject {
                    rubric: "code_review_pr_citation".into(),
                    reason: VerificationReason::SkillRejection {
                        detail: format!(
                            "PR #{pr_num} mentioned without a nearby `[<claim_id>]` citation"
                        ),
                    },
                    evidence: serde_json::json!({
                        "pr_number": pr_num,
                        "window_chars": 120,
                    }),
                };
            }
        }
        baseline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn code_review_overrides_narration_composition_traversal() {
        let s = CodeReviewSkill;
        assert_eq!(s.name(), "code_review");
        assert!(s.section(SynthesisStage::Narration).unwrap()
            .to_lowercase().contains("pr"));
        assert!(s.section(SynthesisStage::Composition).unwrap()
            .contains("Summary"));
        let cfg = s.traversal_config().unwrap();
        assert_eq!(cfg.max_hops, 2);
    }

    #[tokio::test]
    async fn code_review_verifier_rejects_uncited_pr() {
        let s = CodeReviewSkill;
        let a = Uuid::new_v4();
        let narrative = format!("Fixed bug in [{a}]. Opened PR #1234 separately.");
        // The PR is referenced AFTER a long gap — no nearby citation.
        let narrative_padded = format!(
            "Fixed bug in [{a}]. {} Opened PR #1234 separately.",
            "x".repeat(300)
        );
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative_padded,
            cluster_member_ids: &[a],
        };
        match s.verify(&ctx).await {
            VerificationOutcome::Reject {
                reason: VerificationReason::SkillRejection { detail }, ..
            } => assert!(detail.contains("#1234")),
            other => panic!("expected PR-citation reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn code_review_verifier_accepts_pr_with_nearby_citation() {
        let s = CodeReviewSkill;
        let a = Uuid::new_v4();
        let narrative = format!("Fixed bug in [{a}] — opened PR #1234.");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a],
        };
        match s.verify(&ctx).await {
            VerificationOutcome::Accept { .. } => {}
            other => panic!("expected Accept, got {other:?}"),
        }
    }
}
```

- [ ] **Step 1-4**: Same TDD shape as Task 2.2. Tests, register, markdown, commit.

Commit message:

```
feat(synthesis): CodeReviewSkill with PR-citation rubric (Phase 3)

Fourth concrete SynthesisSkill, tuned for the nightly-bug-fix
pipeline. PR-body-shaped narration; verifier adds a check that every
`#NNNN` PR reference appears within 120 chars of a `[<claim_id>]`
citation. Strictness needed because PR-body narratives are merge-gates,
not just summaries.

Migration 5028b extends syntheses_skill_name_known to allow
'code_review'.
```

---

## Phase 4 — Episcience: `RegistryDiffSkill`

Same shape as Phases 2 and 3. Specialisation for the weekly-capability-audit workflow.

### Task 4.1 — Migration

File: `migrations/synthesis/5028c_syntheses_skill_registry_diff.sql`.

### Task 4.2 — `RegistryDiffSkill` impl

Domain: capability-registry diffs.

- `name()` → `"registry_diff"`
- `section(Overview)` → "Summarise a capability-audit run: tools added, tools removed, tools whose schemas drifted."
- `section(Narration)` → "For each cluster, list capability changes. Cite added tools as `+`, removed as `-`, drifted as `~`. Every claim must be cited with `[<claim_id>]`."
- `section(Composition)` → produces three Markdown tables (Added / Removed / Drifted).
- `traversal_config()` → `max_hops=1` (capability claims are usually shallow), `edge_types = [EdgeType::Supersedes]` (tool versions chain).
- `verify()` → additional check: every claim cited under "Removed" must carry an `epigraph_edge_id` property (to prove the removal was wired into the kernel). If the cluster doesn't have that property accessible, fall back to the default rubric — note in the doc comment.

Same TDD shape as Phase 3. Commit message:

```
feat(synthesis): RegistryDiffSkill for capability audits (Phase 4)

Fifth concrete SynthesisSkill, tuned for the weekly-capability-audit
workflow. Diff-shaped narration; three-table composition (Added /
Removed / Drifted); Supersedes-only traversal.

Migration 5028c extends syntheses_skill_name_known to allow
'registry_diff'.
```

---

## Phase 5 — EpiClaw: post-workflow synthesis hook

Goal: when a scheduled task completes successfully AND its workflow_id has a `synthesis_skill` property, POST a synthesis to episcience using that skill.

Fire-and-forget. EpiClaw doesn't wait for the synthesis to complete.

### Task 5.1 — Add the episcience HTTP client

**Files:**
- Create: `/home/jeremy/epiclaw-host-wt-integration/src/host/episcience_client.rs`

The simplest shape: a thin HTTP client wrapping `reqwest`. EpiClaw already has reqwest as a dep (verify with `grep reqwest /home/jeremy/epiclaw-host-wt-integration/Cargo.toml`).

```rust
//! HTTP client for the episcience ELN API. Used by the post-workflow
//! synthesis hook to register workflow_run samples and enqueue
//! syntheses. EpiClaw does NOT block on the synthesis; the calls are
//! fire-and-forget from the perspective of the scheduler's main loop.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct EpiscienceClient {
    base_url: String,
    bearer: String,
    client: reqwest::Client,
}

impl EpiscienceClient {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer: bearer.into(),
            client: reqwest::Client::new(),
        }
    }

    pub async fn create_workflow_run(
        &self,
        workflow_id: Uuid,
        canonical_name: &str,
        prepared_by: Uuid,
        started_at: DateTime<Utc>,
    ) -> Result<Uuid, EpiscienceError> {
        let url = format!("{}/api/v1/eln/workflow_runs", self.base_url);
        let body = CreateWorkflowRunRequest {
            workflow_id,
            canonical_name: canonical_name.into(),
            prepared_by,
            started_at,
            labels: vec![],
        };
        let resp = self.client
            .post(&url)
            .bearer_auth(&self.bearer)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let parsed: CreateWorkflowRunResponse = resp.json().await?;
        Ok(parsed.sample_id)
    }

    pub async fn enqueue_synthesis(
        &self,
        query: &str,
        skill_name: &str,
        agent_id: Uuid,
    ) -> Result<Uuid, EpiscienceError> {
        let url = format!("{}/api/v1/eln/syntheses", self.base_url);
        let body = CreateSynthesisRequest {
            query: query.into(),
            traversal_config: None,
            parent_synthesis_id: None,
            prereq_synthesis_ids: vec![],
            visibility: "shared".into(),
            skill_name: Some(skill_name.into()),
        };
        let resp = self.client
            .post(&url)
            .bearer_auth(&self.bearer)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let parsed: CreateSynthesisResponse = resp.json().await?;
        Ok(parsed.id)
    }
}

#[derive(Debug, Serialize)]
struct CreateWorkflowRunRequest {
    workflow_id: Uuid,
    canonical_name: String,
    prepared_by: Uuid,
    started_at: DateTime<Utc>,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CreateWorkflowRunResponse {
    sample_id: Uuid,
}

#[derive(Debug, Serialize)]
struct CreateSynthesisRequest {
    query: String,
    traversal_config: Option<serde_json::Value>,
    parent_synthesis_id: Option<Uuid>,
    prereq_synthesis_ids: Vec<Uuid>,
    visibility: String,
    skill_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateSynthesisResponse {
    id: Uuid,
}

#[derive(Debug, thiserror::Error)]
pub enum EpiscienceError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}
```

Wire `pub mod episcience_client;` into `src/host/mod.rs`.

### Task 5.2 — Workflow-run hook in the scheduler

**Files:**
- Create: `src/host/workflow_run_hook.rs`
- Modify: `src/host/scheduler.rs`
- Modify: `src/host/config.rs` (add `episcience_url` field, `EPISCIENCE_URL` env var)
- Modify: `src/host/provenance.rs`

The hook fires from `ProvenanceRecorder::record_task_executed()` AFTER the existing claim emission, only when `status == "completed"` AND the task carries a `workflow_id`.

```rust
//! Post-workflow synthesis hook.
//!
//! When a scheduled task completes successfully AND has a workflow_id,
//! and the workflow carries a `synthesis_skill` property, post a
//! workflow_run sample + a synthesis to episcience. Fire-and-forget:
//! episcience runs the pipeline asynchronously; the scheduler does not
//! block.

use chrono::Utc;
use uuid::Uuid;

use crate::host::episcience_client::EpiscienceClient;

pub struct WorkflowRunHook {
    client: EpiscienceClient,
    host_agent_id: Uuid,
}

impl WorkflowRunHook {
    pub fn new(client: EpiscienceClient, host_agent_id: Uuid) -> Self {
        Self { client, host_agent_id }
    }

    /// Fire-and-forget. Errors logged at `warn` level, not propagated.
    pub async fn on_workflow_task_completed(
        &self,
        workflow_id: Uuid,
        canonical_name: &str,
        synthesis_skill: &str,
    ) {
        let started_at = Utc::now();
        let sample_id = match self.client.create_workflow_run(
            workflow_id, canonical_name, self.host_agent_id, started_at,
        ).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "episcience workflow_run create failed (non-fatal)"
                );
                return;
            }
        };
        let query = format!(
            "workflow_run:{} canonical:{} sample:{}",
            workflow_id, canonical_name, sample_id
        );
        match self.client.enqueue_synthesis(
            &query, synthesis_skill, self.host_agent_id,
        ).await {
            Ok(synthesis_id) => {
                tracing::info!(
                    workflow_id = %workflow_id,
                    sample_id = %sample_id,
                    synthesis_id = %synthesis_id,
                    skill = synthesis_skill,
                    "enqueued post-workflow synthesis"
                );
            }
            Err(e) => {
                tracing::warn!(
                    workflow_id = %workflow_id,
                    error = %e,
                    "episcience synthesis enqueue failed (non-fatal)"
                );
            }
        }
    }
}
```

Wire into `scheduler.rs::execute_scheduled_task` (or whichever function is called after the container exits): after the existing `record_task_executed` call, if the task has a non-None `workflow_id` and the workflow's `properties.synthesis_skill` exists, call `WorkflowRunHook::on_workflow_task_completed`.

The workflow's properties may not be in EpiClaw's local state — EpiClaw fetches the workflow on demand via `api.get_workflow(workflow_id)`. Add a thin extension: `api.get_workflow_synthesis_skill(workflow_id) -> Option<String>`. If `None`, skip the hook (no synthesis is requested for this workflow).

### Task 5.3 — Tests

Two integration tests in `epiclaw-host-wt-integration/tests/`:

1. `workflow_run_hook_fires_on_success` — mock the episcience HTTP server, run a scheduled task that completes, assert the hook POSTed to `/eln/workflow_runs` AND `/eln/syntheses`.
2. `workflow_run_hook_skips_on_missing_skill` — same setup but the workflow's `properties.synthesis_skill` is None; assert the hook does NOT call episcience.

Use `wiremock` or `mockito` — verify which EpiClaw already has as a dev-dep. Otherwise stand up an actual axum test server with two test routes.

### Task 5.4 — Commit + push

Commit:

```
feat(scheduler): post-workflow synthesis hook (Phase 5)

When a scheduled task completes AND its workflow has properties.synthesis_skill,
post a workflow_run sample + a synthesis to episcience. Fire-and-forget.

Requires episcience PR #1 (Phase 1 workflow_run route) merged and the
episcience server reachable at $EPISCIENCE_URL.
```

PR title: `feat(scheduler): post-workflow synthesis hook (Phase 5)`.

---

## Phase 6 — EpiClaw: `add_observation` for task outputs

Goal: alongside the existing `task_executed` provenance claim, emit a richer `add_observation` call against the workflow_run sample.

### Task 6.1 — Add `add_observation` to EpiscienceClient

Extend `episcience_client.rs` with:

```rust
pub async fn add_observation(
    &self,
    sample_id: Uuid,
    content: &str,
    relationship: &str,  // "observation" | "measurement" | "characterization" | "preparation_note"
    agent_id: Uuid,
) -> Result<Uuid, EpiscienceError> {
    let url = format!("{}/api/v1/eln/samples/{}/observations", self.base_url, sample_id);
    let body = serde_json::json!({
        "content": content,
        "agent_id": agent_id,
        "relationship": relationship,
    });
    let resp = self.client
        .post(&url)
        .bearer_auth(&self.bearer)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let parsed: serde_json::Value = resp.json().await?;
    Ok(parsed["claim_id"].as_str().unwrap().parse().unwrap())
}
```

### Task 6.2 — Wire into `ProvenanceRecorder::record_task_executed`

After emitting the existing telemetry claim, also call `add_observation` with the task's output_summary content and `relationship = "observation"`. The sample_id is the workflow_run sample created by Phase 5's hook (cached on the task's run-state struct).

If `sample_id` is None (no workflow_run sample was created — e.g., the workflow has no synthesis_skill), skip.

### Task 6.3 — Tests

One integration test asserting: after a workflow run that produces output, the episcience samples table has a claim row tied via `sample_claims` to the workflow_run sample with `relationship = 'observation'`.

### Task 6.4 — Commit + PR

Commit: `feat(provenance): add_observation for task outputs (Phase 6)`.

---

## Phase 7 — EpiClaw: `attach_blob` for `/workspace/group/` artifacts

Goal: every file an EpiClaw container writes to its group folder becomes a content-addressed blob attached to the workflow_run sample.

### Task 7.1 — Add `attach_blob` to EpiscienceClient

The episcience MCP tool takes base64 + filename + mime_type. The HTTP route equivalent is `POST /api/v1/eln/blobs` as multipart. The simpler client-side path: base64 + JSON.

If the route only supports multipart, use multipart from the EpiClaw client side too. Check `crates/episcience-api/src/routes/blobs.rs` for the surface.

```rust
pub async fn attach_blob(
    &self,
    file_bytes: &[u8],
    filename: &str,
    mime_type: &str,
    sample_id: Option<Uuid>,
    uploader_id: Uuid,
) -> Result<Uuid, EpiscienceError> {
    let url = format!("{}/api/v1/eln/blobs", self.base_url);
    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_bytes.to_vec())
                .file_name(filename.to_string())
                .mime_str(mime_type)?,
        )
        .text("uploader_id", uploader_id.to_string());
    let form = if let Some(sid) = sample_id {
        form.text("sample_id", sid.to_string())
    } else {
        form
    };
    let resp = self.client
        .post(&url)
        .bearer_auth(&self.bearer)
        .multipart(form)
        .send()
        .await?
        .error_for_status()?;
    let parsed: serde_json::Value = resp.json().await?;
    Ok(parsed["id"].as_str().unwrap().parse().unwrap())
}
```

### Task 7.2 — Container exit hook scans group folder

In `src/host/container.rs`, after the container exits successfully, list new/modified files under the group folder (compare mtimes or use a manifest). For each, read bytes + guess mime type, call `attach_blob`. Cap the size (e.g., skip > 50 MB) and the count (e.g., max 50 files per run) to avoid runaway uploads.

Use the existing `mime_guess` crate or simple extension-based mapping for the mime type — keep it conservative; default to `application/octet-stream`.

### Task 7.3 — Tests

End-to-end: run a scheduled task that writes 2-3 files to `/workspace/group/`. After the container exits, assert episcience has 2-3 blobs tied to the workflow_run sample.

### Task 7.4 — Commit + PR

Commit: `feat(container): attach_blob for /workspace/group/ artifacts (Phase 7)`.

---

## Phase 8 — Shared: countersign-as-merge-gate

Goal: turn the synthesis-of-PR-narrative into a real merge gate. A second agent runs the CodeReviewSkill verifier against the PR's synthesis; on accept, calls `countersign(claim_id, signature_meaning='approved')`; the nightly-bug-fix workflow only flips `gh pr ready` if the approved countersignature exists.

This phase depends on Phase 3 (CodeReviewSkill) AND Phase 5 (workflow_run hook). It's the most architecturally significant of the medium-leverage opportunities.

### Task 8.1 — `episcience-review-bot` container

A new EpiClaw scheduled task (or a separate container entirely) that:

1. Subscribes to syntheses emitted by the nightly-bug-fix workflow (poll the episcience HTTP API for syntheses with `skill_name='code_review'` and `status='complete'`).
2. For each, re-runs the verifier rubric (the synthesis is already accepted by the worker, but the review bot does a second independent check — different agent identity, same rubric).
3. On accept, calls `countersign(claim_id, signature_meaning='approved', signature_hex=<Ed25519 sig of the canonical message>, public_key_hex=<this bot's pubkey>)`.
4. The nightly-bug-fix workflow's final step: query for `approved` countersignatures on the synthesis's narrative claim; only mark PR ready when present.

This is several pieces:

- **Bot impl**: 1 new file in EpiClaw or a separate small crate.
- **Bot key material**: new agent registered in EpiGraph with `agent_type='reviewer'`.
- **PR-ready gate**: 1 new step in the nightly-bug-fix workflow's stored definition (read by the bug-fix container at run time).

Treat this as a multi-task phase. Key tasks:

- [ ] **Task 8.1.1** — Register the review-bot agent in EpiGraph (one-time setup via `mcp__epigraph__memorize` of the bot's identity).
- [ ] **Task 8.1.2** — Add `episcience_review_bot::review_pending_syntheses()` that lists `code_review`-skill syntheses with `status='complete'` and no `approved` countersignature.
- [ ] **Task 8.1.3** — For each, re-run CodeReviewSkill::verify on the narrative; if accept, call `countersign`.
- [ ] **Task 8.1.4** — Add a scheduler entry in `schedules.toml` for the review bot (interval = 5 minutes; group = `review`; container_timeout_ms = 60s).
- [ ] **Task 8.1.5** — Modify the nightly-bug-fix workflow's stored prompt: append "Before calling `gh pr ready`, run `mcp__episcience__list_countersignatures(claim_id=<synthesis_narrative_claim_id>)` and verify ≥ 1 row with `signature_meaning='approved'`."

Each is ~30 minutes of work. The whole phase is ~half a day.

Commit messages:

- `feat(review-bot): episcience review bot for code_review syntheses (Phase 8.1)`
- `feat(review-bot): list pending syntheses needing approval (Phase 8.2)`
- `feat(review-bot): countersign on verifier accept (Phase 8.3)`
- `feat(scheduler): review-bot 5-min interval (Phase 8.4)`
- `chore(workflows): nightly-bug-fix gates ready on countersignature (Phase 8.5)`

---

## Phase 9 — Episcience: `PaperNoveltyBackend`

Goal: a `NoveltyBackend` impl that scores ingestion-candidate papers against prior syntheses + prior claims-with-DOIs, instead of just prior syntheses (which is what `InternalNoveltyBackend` does today).

### Task 9.1 — `PaperNoveltyBackend` impl

**Files:**
- Create: `crates/episcience-db/src/synthesis/novelty_backend_paper.rs`

Same `NoveltyBackend` trait as `InternalNoveltyBackend` (from PR #6). The score function:

```rust
async fn score(
    &self,
    candidate_id: Uuid,
    candidate_narrative: &str,
    candidate_member_ids: &[Uuid],
) -> Result<NoveltyScore, NoveltyError> {
    // 1. Score against prior syntheses (delegate to InternalNoveltyBackend).
    let internal_score = InternalNoveltyBackend {
        pool: self.pool.clone(),
        embedder: self.embedder.clone(),
    }.score(candidate_id, candidate_narrative, candidate_member_ids).await?;

    // 2. Additionally find prior claims with a `doi` label that share
    //    semantic embedding similarity with the candidate. The score
    //    is `min(internal_score, 1.0 - top_doi_similarity)` — both
    //    sources must agree the candidate is novel.
    let cand_emb = self.embedder.generate(candidate_narrative).await
        .map_err(|e| NoveltyError::Unavailable(e.to_string()))?;
    let top_doi_sim = find_top_doi_claim_similarity(&self.pool, &cand_emb).await
        .map_err(|e| NoveltyError::Db(e.to_string()))?;
    let combined = internal_score.score.min(1.0 - top_doi_sim);

    Ok(NoveltyScore {
        score: combined,
        backend: "paper_novelty".into(),
        neighbours: internal_score.neighbours,  // top syntheses neighbours
        rationale: format!(
            "internal_syntheses {:.3}; top_doi_similarity {:.3}; combined {:.3}",
            internal_score.score, top_doi_sim, combined
        ),
    })
}
```

`find_top_doi_claim_similarity` queries `claims` for rows with `'doi'` in `labels`, joins to their embedding (in `claim_embeddings` table — verify the actual table name first), computes cosine, returns max.

### Task 9.2 — Wire the backend into the job handler for `literature`-skill syntheses

In `synthesis_job.rs`, the Stage 7 backend is currently hardcoded to `InternalNoveltyBackend`. Add a skill-dispatch:

```rust
let backend: Box<dyn NoveltyBackend> = match pipeline.skill.name() {
    "literature" => Box::new(PaperNoveltyBackend { pool, embedder }),
    _ => Box::new(InternalNoveltyBackend { pool, embedder }),
};
let novelty = pipeline.stage7_novelty(synthesis_id, &narrative, &cluster_member_ids, backend.as_ref()).await?;
```

### Task 9.3 — Tests

Three tests:
1. `paper_novelty_returns_one_when_no_priors` — no syntheses, no doi claims.
2. `paper_novelty_low_when_doi_match_found` — pre-insert a DOI claim with embedding identical to the candidate's. Assert score < 0.1.
3. `paper_novelty_uses_min_of_two_sources` — pre-insert both a prior synthesis with high overlap AND a DOI claim with low similarity. Assert the score reflects the syntheses overlap (lower of the two).

### Task 9.4 — Commit + PR

Commit: `feat(synthesis): PaperNoveltyBackend scores against prior DOI claims (Phase 9)`.

---

## Phase 10 — Shared: ProtocolSections-aligned schedules.toml

Goal: align EpiClaw's `schedules.toml` schema with episcience's `ProtocolSections` vocabulary (overview / planning / implementation / interpretation / validation). Additive — pre-existing schedules.toml entries continue to work.

### Task 10.1 — Extend schedules.toml schema

**Files:**
- Modify: `src/host/scheduler_db.rs` (the `ScheduledTask` struct + the deserializer for schedules.toml entries)

Add an optional `sections: Option<ProtocolSections>` field to the `ScheduledTask` struct, structured the same as episcience's `ProtocolSections` (overview/planning/implementation/interpretation/validation/extras). Today's `prompt` field maps semantically to `sections.implementation` — accept either form, with `prompt` interpreted as `sections.implementation` when only `prompt` is present.

### Task 10.2 — Round-trip test

`schedules.toml` example:

```toml
[[schedules]]
id = "research-scan-morning"
schedule_type = "cron"
schedule_value = "0 9 * * *"
group = "research"

[schedules.sections]
overview = "Weekly arxiv scan for new papers in the project's research areas."
planning = "List the arxiv categories and date range. Confirm openai api key is available."
implementation = "Run /workspace/group/scan-arxiv.sh with the configured categories."
interpretation = "For each paper, decide ingest / skip / queue-for-review."
validation = "Confirm each ingested paper appears via mcp__epigraph__query_paper(doi)."
```

Test: load this TOML, assert the parsed ScheduledTask has all 5 sections + the legacy `prompt` field (derived from `implementation`).

### Task 10.3 — Surface sections to the container

When the container is spawned, the prompt it receives currently is just the `prompt` field. After this change, the agent receives the FULL prompt with the section headers spliced in — same shape as a `Protocol` row in episcience uses. The agent can then reference `[VALIDATION]` etc. in its own decisions.

### Task 10.4 — Commit + PR

Commit: `feat(scheduler): ProtocolSections-aligned schedules.toml (Phase 10)`.

---

## Phase 11 — Documentation + finishing

Goal: bring episcience and EpiClaw docs in sync with the integration.

### Task 11.1 — Episcience docs

**Files:**
- Modify: `/home/jeremy/episcience/docs/intro/02-concepts-science.md`
- Modify: `/home/jeremy/episcience/docs/intro/05-workflows.md`
- Modify: `/home/jeremy/episcience/docs/intro/04-glossary.md`

Additions:

- `02-concepts-science.md` §12 (Workflow runs): the sample_type='workflow_run' shape, how it ties to EpiClaw workflows, what claims attach via sample_claims.
- `02-concepts-science.md` §13 (Per-workflow synthesis skills): introduce LiteratureSkill, CodeReviewSkill, RegistryDiffSkill with their differentiators.
- `05-workflows.md` Workflow D: "EpiClaw nightly-bug-fix → episcience review-bot → countersigned PR" (end-to-end across Phase 8).
- `04-glossary.md`: new terms — workflow_run sample, literature skill, code_review skill, registry_diff skill, review bot, countersign-as-merge-gate.

### Task 11.2 — EpiClaw docs

**Files:**
- Create: `/home/jeremy/epiclaw-host/docs/integration-with-episcience.md`
- Modify: `/home/jeremy/epiclaw-host/README.md`

The new doc covers:
- Environment vars: `EPISCIENCE_URL`, `EPISCIENCE_SERVICE_TOKEN`.
- The post-workflow synthesis hook lifecycle.
- How to opt a workflow IN to synthesis (set `workflow.properties.synthesis_skill`).
- Operational guidance: how to debug a missing synthesis (look for `tracing::warn!` lines).
- Cross-references to episcience's plan + concepts docs.

README addition: one paragraph linking to the new doc.

### Task 11.3 — Commit + PR

Commit (in episcience):

```
docs(intro): EpiClaw integration coverage (Phase 11)
```

Commit (in epiclaw-host):

```
docs: episcience integration guide (Phase 11)
```

### Task 11.4 — Final code review

After all 11 phases ship, request a final code-quality review across the merged surface via `superpowers:requesting-code-review`.

### Task 11.5 — Finish branch

Use `superpowers:finishing-a-development-branch` for any worktrees still open.

---

## Out of scope

These were considered and deliberately deferred:

1. **Real-time push of syntheses from episcience back to EpiClaw.** Today the integration is one-way (EpiClaw pushes to episcience). Adding a webhook so episcience can notify EpiClaw "synthesis ready" would be a Phase 12 — meaningful but not load-bearing for the first ship.
2. **Bidirectional `synthesis_provo_edges.target_kind = 'workflow'`.** Linking workflow generation chains to synthesis refinement chains via shared REFINES predicate. The infrastructure exists but no caller. Defer until a third caller wants the shape.
3. **Replacing EpiClaw's SQLite scheduler with EpiGraph workflows.** Massive refactor; not in scope.
4. **Custom verifier rubrics per workflow via stored configuration.** Today each skill encodes its own rubric in Rust. Making rubrics data-driven (e.g., a JSON DSL stored on the workflow row) is interesting but premature.

---

## Self-review against the source analysis

| Source opportunity (from `/home/jeremy/episcience/...analysis...`) | Plan coverage |
|---|---|
| 1. Synthesize the workflow run | Phases 1 + 5 |
| 2. Custom SynthesisSkill per workflow | Phases 2 (literature) + 3 (code_review) + 4 (registry_diff) |
| 3. Countersign + verifier as merge gate | Phase 8 |
| 4. Novelty for ingest workflows | Phase 9 |
| 5. Refinement chain for low-quality runs | Implicit — refinement is already in main (Phase 7 of the SciLink-lessons plan). When the new skills reject, refinement spawns naturally. |
| 6. add_observation for task outputs | Phase 6 |
| 7. attach_blob for task artifacts | Phase 7 |
| 8. ProtocolSections vocab for schedules.toml | Phase 10 |
| 9. Shared REFINES semantics for workflows | Out of scope |
| 10. Autonomy levels + meta-agent run_task | Out of scope (already deferred in the SciLink-lessons plan) |

No gaps in the highest- and medium-leverage tier. Two architectural opportunities (9, 10) deliberately deferred.

---

## Recommended execution order

1. **Phase 0** (episcience + epiclaw): branches set up.
2. **Phase 1** (episcience): workflow_run route. Ships as one PR.
3. **Phase 2** (episcience): LiteratureSkill. Ships as one PR.
4. **Phase 5** (epiclaw): synthesis hook. **First end-to-end demo lands here** — a workflow run now produces a verifier-accepted, novelty-scored narrative.
5. **Phase 3** + **Phase 4** (episcience): CodeReviewSkill and RegistryDiffSkill. Independent PRs, any order.
6. **Phase 6** + **Phase 7** (epiclaw): add_observation and attach_blob. Independent PRs.
7. **Phase 8** (shared): countersign-as-merge-gate. The architecturally most significant phase; depends on 3 + 5.
8. **Phase 9** (episcience): PaperNoveltyBackend.
9. **Phase 10** (shared): ProtocolSections-aligned schedules.toml.
10. **Phase 11** (both): docs + finishing.

Each phase is one PR (or one PR per repo for shared phases). After Phase 5 lands, the integration is *operationally* useful — every subsequent phase adds depth.
