# SciLink Lessons → Episcience Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the high-leverage architectural patterns from [SciLink](https://github.com/ziatdinovmax/SciLink) into episcience: a `SynthesisSkill` trait + section vocabulary for the synthesis worker, a verifier-driven acceptance stage, a novelty assessment stage, simulated-annealing refinement via the `REFINES` chain, MCP surface parity with HTTP, and a section vocabulary for protocols.

**Architecture:** Refactor the existing 5-stage `SynthesisPipeline<L, P>` into a 7-stage pipeline parameterised by a `SynthesisSkill` trait that contributes per-stage prompt sections, traversal configuration, and a verification rubric. Skills are Rust types (with optional markdown content under `crates/episcience-core/skills/`), selected per-synthesis via the `syntheses.properties.skill` JSON field. A new stage 6 (`stage6_verify`) gates `status = 'complete'` on verifier acceptance; failures route to refinement with a progressively-thawing `RefinementTemperature` carried by the `REFINES` chain. A new stage 7 (`stage7_novelty`) scores the synthesis against prior syntheses and (pluggably) external literature. The MCP server gains write tools that mirror the HTTP routes. Protocols gain a structured section vocabulary additively.

**Tech Stack:** Rust (axum 0.7, sqlx 0.7, tokio, async-trait 0.1), PostgreSQL 16, BLAKE3, Ed25519, MCP (rmcp).

**Repo:** `/home/jeremy/episcience/`
**Dev DB:** `postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_dev`
**Test DB:** `postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test`
**Reference:** `/home/jeremy/reference/SciLink/CLAUDE.md` (architectural notes)

---

## Scope and lesson disposition

SciLink contributes nine architectural lessons. This plan covers the seven that belong to episcience. Two go elsewhere — they are out-of-scope and listed at the bottom of this document so engineers don't re-introduce them here.

| # | Lesson | Owner | In this plan? |
|---|---|---|---|
| 1 | Foundation-agent pipeline + skill bundle pattern | Episcience | ✅ Phases 1–3, 5 |
| 2 | Verifier-driven acceptance before `status = complete` | Episcience | ✅ Phase 4 |
| 3 | Skill bundle (markdown + Rust impl) for synthesis specialisations | Episcience | ✅ Phases 1, 5 |
| 4 | Three autonomy levels for agents (co-pilot / autopilot / autonomous) | **EpiClaw** | ❌ separate plan |
| 5 | MCP surface parity with HTTP write paths | Episcience | ✅ Phase 8 |
| 6 | Novelty assessment as a pipeline stage | Episcience | ✅ Phase 6 |
| 7 | Meta-agent `run_task(task, context)` contract for chaining | **EpiClaw** | ❌ separate plan |
| 8 | Section vocabulary for protocols | Episcience | ✅ Phase 9 |
| 9 | Simulated-annealing refinement (anneal priors on failure) | Episcience | ✅ Phase 7 |

**EpiGraph kernel:** no changes. The skill-bundle abstraction is intentionally an application-level concern in SciLink, and we mirror that. Re-evaluate if a second downstream app (besides episcience) needs skills — at that point a generic content-addressed `bundles` kernel table becomes warranted, but it is YAGNI today.

**Phasing recommendation:** phases 0–4 form the load-bearing foundation and should ship as one PR (the verifier stage is meaningless without the skill trait). Phases 5, 6, 7, 8, 9 are independent and can each be a separate PR, in any order — but the order below maximises the value-per-week. Each phase ends with a green test run, a successful migration apply, and a commit.

---

## File structure

### New files

```
crates/episcience-core/src/synthesis/skill.rs            — SynthesisStage + SynthesisSkill trait
crates/episcience-core/src/synthesis/skills/baseline.rs  — BaselineSkill (default; current behaviour)
crates/episcience-core/src/synthesis/skills/lab_notebook.rs — LabNotebookSkill (Phase 5)
crates/episcience-core/src/synthesis/skills/mod.rs       — Skill registry + load_by_name()
crates/episcience-core/src/synthesis/verifier.rs         — VerifierOutcome + VerificationContext
crates/episcience-core/src/synthesis/novelty.rs          — NoveltyScore + NoveltyBackend trait
crates/episcience-core/src/synthesis/refinement.rs       — RefinementTemperature + anneal()
crates/episcience-db/src/synthesis/skill_loader.rs       — Resolve skill name from syntheses row
crates/episcience-db/src/synthesis/novelty_backend_internal.rs — Default backend: prior-syntheses similarity
crates/episcience-api/src/mcp/eln_writes.rs              — MCP tools mirroring HTTP write paths (Phase 8)
migrations/5019_syntheses_skill_column.sql               — Add skill, verifier_outcome columns
migrations/5020_syntheses_novelty.sql                    — Add novelty_score, novelty_backend columns
migrations/5021_syntheses_refinement_temperature.sql     — Add refinement_temperature column
migrations/5022_protocols_section_vocabulary.sql         — Add overview/planning/...
crates/episcience-core/src/synthesis/skills/markdown/baseline.md       — Skill markdown reference
crates/episcience-core/src/synthesis/skills/markdown/lab_notebook.md   — Skill markdown reference
```

### Modified files

```
crates/episcience-db/src/synthesis/pipeline.rs           — Parameterise on S: SynthesisSkill; add stage6/7
crates/episcience-api/src/jobs/synthesis_job.rs          — Resolve and inject skill; route on verifier
crates/episcience-api/src/routes/syntheses.rs            — Accept skill name in POST body
crates/episcience-api/src/mcp/mod.rs                     — Register new MCP write tools (Phase 8)
crates/episcience-api/src/routes/protocols.rs            — Accept structured sections (Phase 9)
crates/episcience-core/src/protocol.rs                   — ProtocolSections struct (Phase 9)
crates/episcience-core/src/lib.rs                        — Re-export new modules
crates/episcience-core/src/synthesis/mod.rs              — Re-export skill / verifier / novelty
```

---

## Phase 0 — Branch and harness

**Files:**
- N/A (git only)

- [ ] **Step 1: Verify worktree origin and branch from public**

```bash
cd /home/jeremy/episcience && git remote -v
# Expected: origin -> github.com/epigraph-io/episcience (public)
git fetch origin && git checkout -b feat/scilink-skill-foundation origin/main
```

- [ ] **Step 2: Confirm baseline tests pass before any changes**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
  cargo test --workspace --lib --bins
```

Expected: all green. If anything is red, **stop and fix on a separate branch first** — do not start the refactor on a broken baseline.

- [ ] **Step 3: Record baseline test count**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
  cargo test --workspace --lib 2>&1 | tail -5
# Note the "test result: ok. N passed" line — keep N for regression compare at end of each phase.
```

- [ ] **Step 4: Commit branch creation marker (empty commit)**

```bash
git commit --allow-empty -m "chore: begin SciLink skill-foundation work"
```

---

## Phase 1 — `SynthesisSkill` trait + `BaselineSkill`

Goal: introduce the trait and a zero-behaviour-change default impl. No pipeline call sites change yet — this phase is pure additive scaffolding so the test surface stays identical.

### Task 1.1 — Define `SynthesisStage` enum

**Files:**
- Create: `crates/episcience-core/src/synthesis/skill.rs`
- Modify: `crates/episcience-core/src/synthesis/mod.rs` (add `pub mod skill`)
- Test: `crates/episcience-core/src/synthesis/skill.rs` (inline `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing test**

Append to `crates/episcience-core/src/synthesis/skill.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesis_stage_round_trips_through_str() {
        for s in [
            SynthesisStage::Overview,
            SynthesisStage::Planning,
            SynthesisStage::Traversal,
            SynthesisStage::Clustering,
            SynthesisStage::Narration,
            SynthesisStage::Composition,
            SynthesisStage::Verification,
            SynthesisStage::Novelty,
        ] {
            let serialized = s.as_str();
            let parsed = SynthesisStage::from_str(serialized)
                .unwrap_or_else(|| panic!("could not parse {serialized}"));
            assert_eq!(parsed, s, "round-trip failed for {serialized}");
        }
    }

    #[test]
    fn synthesis_stage_rejects_unknown_strings() {
        assert!(SynthesisStage::from_str("not_a_stage").is_none());
        assert!(SynthesisStage::from_str("").is_none());
    }
}
```

- [ ] **Step 2: Run it to confirm it fails**

```bash
cargo test -p episcience-core synthesis::skill -- --nocapture
```

Expected: FAIL with `cannot find type SynthesisStage`.

- [ ] **Step 3: Implement the enum**

Replace the file body (above the test module) with:

```rust
//! Synthesis-stage section vocabulary and the [`SynthesisSkill`] trait.
//!
//! SciLink's foundation-agent pattern (see SciLink `CLAUDE.md`, "Foundation
//! agents") defines a fixed *section vocabulary* per modality and pluggable
//! *skills* that contribute per-section content. Episcience adopts that
//! pattern for the synthesis worker: [`SynthesisStage`] is the section
//! vocabulary, [`SynthesisSkill`] is the contract a skill implements.

/// The fixed section vocabulary the synthesis pipeline knows how to splice
/// skill-provided content into. The enum is **closed** — adding a new
/// variant is a deliberate pipeline change.
///
/// The naming mirrors SciLink's `overview / planning / implementation /
/// interpretation / validation` set, extended with the stages specific to
/// graph-clustering synthesis (`traversal`, `clustering`, `composition`,
/// `novelty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SynthesisStage {
    Overview,
    Planning,
    Traversal,
    Clustering,
    Narration,
    Composition,
    Verification,
    Novelty,
}

impl SynthesisStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Planning => "planning",
            Self::Traversal => "traversal",
            Self::Clustering => "clustering",
            Self::Narration => "narration",
            Self::Composition => "composition",
            Self::Verification => "verification",
            Self::Novelty => "novelty",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "overview" => Self::Overview,
            "planning" => Self::Planning,
            "traversal" => Self::Traversal,
            "clustering" => Self::Clustering,
            "narration" => Self::Narration,
            "composition" => Self::Composition,
            "verification" => Self::Verification,
            "novelty" => Self::Novelty,
            _ => return None,
        })
    }
}
```

- [ ] **Step 4: Wire module into `synthesis/mod.rs`**

Open `crates/episcience-core/src/synthesis/mod.rs` and add `pub mod skill;` alongside the existing `pub mod ...` declarations.

- [ ] **Step 5: Run the test to confirm it passes**

```bash
cargo test -p episcience-core synthesis::skill
```

Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/episcience-core/src/synthesis/skill.rs \
        crates/episcience-core/src/synthesis/mod.rs
git commit -m "feat(synthesis): add SynthesisStage section vocabulary

Introduce the closed enum of pipeline stages that skills can contribute
prompt sections to, mirroring SciLink's foundation-agent section
vocabulary. No pipeline call sites change yet."
```

### Task 1.2 — Define `SynthesisSkill` trait

**Files:**
- Modify: `crates/episcience-core/src/synthesis/skill.rs`
- Modify: `crates/episcience-core/Cargo.toml` (add `async-trait` if not already a dep — it already is via the synthesis pipeline; verify with `grep async-trait crates/episcience-core/Cargo.toml`)

- [ ] **Step 1: Write the failing test**

Append into the `tests` module in `skill.rs`:

```rust
    #[derive(Debug)]
    struct StubSkill;

    #[async_trait::async_trait]
    impl SynthesisSkill for StubSkill {
        fn name(&self) -> &'static str { "stub" }

        fn section(&self, stage: SynthesisStage) -> Option<&str> {
            match stage {
                SynthesisStage::Overview => Some("stub overview"),
                _ => None,
            }
        }
    }

    #[test]
    fn stub_skill_returns_overview_only() {
        let s = StubSkill;
        assert_eq!(s.name(), "stub");
        assert_eq!(s.section(SynthesisStage::Overview), Some("stub overview"));
        assert_eq!(s.section(SynthesisStage::Narration), None);
        // Default traversal_config must be None so the pipeline falls back to
        // the caller-supplied or schema default.
        assert!(s.traversal_config().is_none());
    }
```

- [ ] **Step 2: Run it to confirm it fails**

```bash
cargo test -p episcience-core synthesis::skill::tests::stub_skill_returns_overview_only
```

Expected: FAIL with `cannot find trait SynthesisSkill`.

- [ ] **Step 3: Add the trait**

Insert above the `#[cfg(test)]` block in `skill.rs`:

```rust
use crate::synthesis::traversal::TraversalConfig;

/// A pluggable synthesis specialisation. Implementations contribute
/// per-stage prompt sections, optional traversal-config defaults, and
/// optional verification rubrics. The default-method bodies encode the
/// "no opinion" answer — callers fall back to baseline behaviour.
///
/// Trait-object safe: pipelines hold `Arc<dyn SynthesisSkill>`.
#[async_trait::async_trait]
pub trait SynthesisSkill: Send + Sync + std::fmt::Debug {
    /// Stable identifier persisted in `syntheses.properties.skill`.
    /// Lowercase snake_case. Must match the registry key (see
    /// `crate::synthesis::skills::load_by_name`).
    fn name(&self) -> &'static str;

    /// Returns the skill-specific prompt section for `stage`, or `None`
    /// to fall back to the pipeline's baseline prompt. Implementations
    /// return short, focused content — multi-paragraph sections belong
    /// in the sibling markdown reference, not in code.
    fn section(&self, stage: SynthesisStage) -> Option<&str>;

    /// Default traversal config override. `None` means "use the caller's
    /// supplied config or the schema default". Skills with strong domain
    /// opinions (e.g. lab-notebook synthesis wants depth=2, edge_types
    /// limited to `derived_from`+`refutes`) override this.
    fn traversal_config(&self) -> Option<TraversalConfig> { None }
}
```

- [ ] **Step 4: Re-run the test**

```bash
cargo test -p episcience-core synthesis::skill
```

Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/episcience-core/src/synthesis/skill.rs
git commit -m "feat(synthesis): introduce SynthesisSkill trait"
```

### Task 1.3 — Implement `BaselineSkill`

**Files:**
- Create: `crates/episcience-core/src/synthesis/skills/mod.rs`
- Create: `crates/episcience-core/src/synthesis/skills/baseline.rs`
- Create: `crates/episcience-core/src/synthesis/skills/markdown/baseline.md`
- Modify: `crates/episcience-core/src/synthesis/mod.rs` (add `pub mod skills;`)

- [ ] **Step 1: Write the failing test**

Create `crates/episcience-core/src/synthesis/skills/baseline.rs` with:

```rust
//! `BaselineSkill` — the default synthesis specialisation.
//!
//! Encodes the prompt content the pre-skill `SynthesisPipeline` carried
//! inline. Loaded when a synthesis row does not specify a skill, so the
//! refactor is behaviour-preserving.

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};

#[derive(Debug, Default)]
pub struct BaselineSkill;

#[async_trait::async_trait]
impl SynthesisSkill for BaselineSkill {
    fn name(&self) -> &'static str { "baseline" }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview =>
                "Summarise the cluster of related claims. Cite each cluster \
                 member exactly once with `[<claim_id>]`.",
            SynthesisStage::Narration =>
                "Produce a short title and a 2-4 sentence summary. Do not \
                 introduce facts not present in the supplied claim contents.",
            SynthesisStage::Composition =>
                "Weave the per-cluster summaries into one Markdown narrative. \
                 Each cluster summary must appear VERBATIM between its \
                 `<<<CLUSTER:{id}:BEGIN>>>` / `<<<CLUSTER:{id}:END>>>` \
                 sentinels.",
            SynthesisStage::Verification =>
                "Accept a narrative iff every cluster member appears in a \
                 citation and no citation refers to a claim outside the cluster.",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_provides_narration_and_composition() {
        let s = BaselineSkill;
        assert_eq!(s.name(), "baseline");
        assert!(s.section(SynthesisStage::Narration).is_some());
        assert!(s.section(SynthesisStage::Composition).is_some());
        // Stages without baseline content return None.
        assert!(s.section(SynthesisStage::Traversal).is_none());
        assert!(s.section(SynthesisStage::Clustering).is_none());
        assert!(s.section(SynthesisStage::Novelty).is_none());
    }
}
```

Create `crates/episcience-core/src/synthesis/skills/mod.rs`:

```rust
//! Synthesis skill registry.
//!
//! Skills are static — registered at compile time as enum variants of
//! [`SkillKind`]. Adding a new skill is a deliberate change: the variant
//! goes here and the impl goes in a sibling module.

pub mod baseline;

use std::sync::Arc;

use crate::synthesis::skill::SynthesisSkill;

/// Look up a skill by its stable name. Unknown names return `None` so the
/// caller can decide whether to error or fall back to baseline.
pub fn load_by_name(name: &str) -> Option<Arc<dyn SynthesisSkill>> {
    match name {
        "baseline" => Some(Arc::new(baseline::BaselineSkill)),
        _ => None,
    }
}

/// The skill used when a synthesis row does not specify one.
pub fn default_skill() -> Arc<dyn SynthesisSkill> {
    Arc::new(baseline::BaselineSkill)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_by_name_returns_baseline() {
        let s = load_by_name("baseline").expect("baseline must be registered");
        assert_eq!(s.name(), "baseline");
    }

    #[test]
    fn load_by_name_returns_none_for_unknown() {
        assert!(load_by_name("does_not_exist").is_none());
    }

    #[test]
    fn default_skill_is_baseline() {
        assert_eq!(default_skill().name(), "baseline");
    }
}
```

- [ ] **Step 2: Wire into `synthesis/mod.rs`**

Add `pub mod skills;` to `crates/episcience-core/src/synthesis/mod.rs`.

- [ ] **Step 3: Run tests to confirm they pass**

```bash
cargo test -p episcience-core synthesis::skills
```

Expected: 4 passed.

- [ ] **Step 4: Create the markdown reference**

Create `crates/episcience-core/src/synthesis/skills/markdown/baseline.md`:

```markdown
---
name: baseline
description: Default synthesis skill — encodes the pre-skill pipeline's
  built-in prompts. Loaded when a synthesis row does not specify a skill.
---

# Overview

Summarise a cluster of related claims with strict citation discipline.

# Narration

Produce a short title plus a 2–4 sentence summary per cluster. Cite each
cluster member exactly once with `[<claim_id>]`. Do not introduce facts
not present in the supplied claim contents.

# Composition

Weave per-cluster summaries into one Markdown narrative. Each cluster
summary must appear VERBATIM between its `<<<CLUSTER:{id}:BEGIN>>>` /
`<<<CLUSTER:{id}:END>>>` sentinels (the validator enforces this byte-for-byte).

# Verification

Accept a narrative iff:
- every cluster member appears in at least one citation, and
- no citation refers to a claim outside the cluster (hallucinated id).

# Novelty

Baseline does not score novelty. Subclasses override.
```

> Note: the markdown is reference for human readers and is not loaded by the Rust code today. SciLink loads markdown at runtime; episcience starts simpler and may add a `MarkdownSkill` loader in a later phase.

- [ ] **Step 5: Commit**

```bash
git add crates/episcience-core/src/synthesis/skills/ \
        crates/episcience-core/src/synthesis/mod.rs
git commit -m "feat(synthesis): add BaselineSkill + skill registry

BaselineSkill ports the pre-skill pipeline's inline prompts into the new
SynthesisSkill contract. Registry lookup is closed-set today; a markdown
loader is a later phase."
```

---

## Phase 2 — Wire skill selection through pipeline + DB

Goal: persist a skill name per synthesis row and inject the resolved `Arc<dyn SynthesisSkill>` into `SynthesisPipeline`. No pipeline behaviour changes yet — the skill is held but not yet consulted.

### Task 2.1 — Migration: `syntheses.skill_name`

**Files:**
- Create: `migrations/5019_syntheses_skill_column.sql`

- [ ] **Step 1: Write the migration**

```sql
-- 5019_syntheses_skill_column.sql
-- Persist the synthesis skill used to drive the pipeline. NULL means
-- "baseline" was used (the default before this column existed).
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS skill_name TEXT;

-- New rows default to 'baseline' so we can drop the NULL = baseline
-- ambiguity once existing rows are backfilled. Backfill is below.
ALTER TABLE syntheses
    ALTER COLUMN skill_name SET DEFAULT 'baseline';

UPDATE syntheses SET skill_name = 'baseline' WHERE skill_name IS NULL;

ALTER TABLE syntheses
    ALTER COLUMN skill_name SET NOT NULL;

-- Constrain to currently-registered skills. Adding a new skill is a new
-- migration that extends this list (deliberate co-evolution).
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline'));
```

- [ ] **Step 2: Apply the migration to the test DB**

```bash
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -f migrations/5019_syntheses_skill_column.sql
```

Expected: `ALTER TABLE`, `UPDATE N`, `ALTER TABLE`, `ALTER TABLE` printed without error.

- [ ] **Step 3: Verify schema**

```bash
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -c "\d syntheses" | grep skill_name
```

Expected: `skill_name | text | not null default 'baseline'::text`.

- [ ] **Step 4: Commit**

```bash
git add migrations/5019_syntheses_skill_column.sql
git commit -m "feat(db): add syntheses.skill_name (default 'baseline')"
```

### Task 2.2 — Parameterise `SynthesisPipeline` on `S: SynthesisSkill`

The existing struct is `SynthesisPipeline<L, P>`. Adding a third parameter would ripple through every call site. We instead hold `Arc<dyn SynthesisSkill>` as a regular field — dynamic dispatch at the per-stage section() boundary is cheap (a few calls per synthesis run, not per claim).

**Files:**
- Modify: `crates/episcience-db/src/synthesis/pipeline.rs`
- Test: `crates/episcience-db/src/synthesis/pipeline.rs` (existing test module)

- [ ] **Step 1: Write the failing test**

Find the existing pipeline tests (`#[cfg(test)] mod tests` block near the bottom of `pipeline.rs`). Add at the end of that module:

```rust
    #[tokio::test]
    async fn pipeline_carries_baseline_skill_by_default() {
        use episcience_core::synthesis::skills::default_skill;
        use std::sync::Arc;

        let pool = test_pool().await;
        let embedder: Arc<dyn epigraph_embeddings::EmbeddingService> =
            Arc::new(epigraph_embeddings::MockEmbeddingService::default());
        let pipeline = SynthesisPipeline::new(
            pool,
            embedder,
            crate::synthesis::pipeline::tests::stub_llm(),
            crate::synthesis::pipeline::tests::stub_edge_provider(),
            vec![],
            20,
        );
        assert_eq!(pipeline.skill.name(), default_skill().name());
    }
```

(If `test_pool`, `stub_llm`, or `stub_edge_provider` is named differently in this codebase, use the actual helper names found in the surrounding test module — the goal is to construct a `SynthesisPipeline` and read its `skill` field.)

- [ ] **Step 2: Run it to confirm it fails**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-db synthesis::pipeline::tests::pipeline_carries_baseline_skill_by_default
```

Expected: FAIL with `no field skill on type SynthesisPipeline`.

- [ ] **Step 3: Add the field and constructor parameter**

In `crates/episcience-db/src/synthesis/pipeline.rs`, edit the struct (around line 58):

```rust
pub struct SynthesisPipeline<L, P> {
    pub pool: PgPool,
    pub embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
    pub llm_client: L,
    pub edge_provider: P,
    pub query_embedding: Vec<f32>,
    pub subgraph_metadata: serde_json::Value,
    pub llm_call_count: u32,
    pub cost_budget: u32,
    /// Skill that contributes per-stage prompt sections and the
    /// verification rubric. Defaults to `BaselineSkill` for
    /// behaviour-preserving construction; callers wanting another skill
    /// use [`Self::with_skill`].
    pub skill: Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>,
}
```

Update `impl<L, P> SynthesisPipeline<L, P> { pub fn new(...) }` (around line 80) to initialise `skill` to the default:

```rust
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
    /// handler after resolving `syntheses.skill_name`.
    pub fn with_skill(
        mut self,
        skill: Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>,
    ) -> Self {
        self.skill = skill;
        self
    }
```

- [ ] **Step 4: Run the test**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-db synthesis::pipeline::tests::pipeline_carries_baseline_skill_by_default
```

Expected: PASS.

- [ ] **Step 5: Run the full workspace tests to confirm no regressions**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test --workspace --lib --bins
```

Expected: same passing count as the Phase 0 baseline + 1 new test.

- [ ] **Step 6: Commit**

```bash
git add crates/episcience-db/src/synthesis/pipeline.rs
git commit -m "feat(synthesis): hold Arc<dyn SynthesisSkill> on the pipeline

Default to BaselineSkill so behaviour is preserved; with_skill() lets
the job handler swap in a row-specific skill after Phase 2.3."
```

### Task 2.3 — Resolve skill name in the job handler

**Files:**
- Modify: `crates/episcience-api/src/jobs/synthesis_job.rs`

- [ ] **Step 1: Write the failing test**

In the test module at the bottom of `synthesis_job.rs` (or sibling test file), add:

```rust
#[tokio::test]
async fn job_handler_resolves_skill_name_from_row() {
    use episcience_core::synthesis::skills;

    let pool = test_pool().await;
    let synthesis_id = insert_test_synthesis(&pool, "baseline").await;
    let resolved = resolve_skill_for_row(&pool, synthesis_id).await.unwrap();
    assert_eq!(resolved.name(), "baseline");

    // Unknown skill names fall back to baseline (and we log a warning).
    let bad_id = insert_test_synthesis_raw_skill(&pool, "unknown_skill_x").await;
    let resolved = resolve_skill_for_row(&pool, bad_id).await.unwrap();
    assert_eq!(resolved.name(), "baseline");
}
```

(If `insert_test_synthesis_raw_skill` would violate the CHECK constraint from Task 2.1, use `sqlx::query!` with a raw insert that bypasses the model layer — or temporarily relax the constraint for the test and reinstate it. Prefer keeping the CHECK strict and writing the test to use a known-bad name *after* the CHECK is extended in Phase 5.)

For now, replace the second half of the test with the comment `// Unknown-name fallback exercised in Phase 5 once a second skill exists.`

- [ ] **Step 2: Run it to confirm it fails**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-api jobs::synthesis_job
```

Expected: FAIL with `cannot find function resolve_skill_for_row`.

- [ ] **Step 3: Implement the resolver**

Add to `synthesis_job.rs` (in the module body, above the `impl JobHandler` block):

```rust
/// Resolve `syntheses.skill_name` for `id` into a concrete skill. Unknown
/// names fall back to baseline so a typo or stale row never blocks the
/// worker; a warning is logged for ops visibility.
pub async fn resolve_skill_for_row(
    pool: &PgPool,
    id: Uuid,
) -> Result<Arc<dyn episcience_core::synthesis::skill::SynthesisSkill>, JobError> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT skill_name FROM syntheses WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| JobError::Db(e.to_string()))?;

    let name = row.ok_or_else(|| JobError::NotFound(id.to_string()))?;
    match episcience_core::synthesis::skills::load_by_name(&name) {
        Some(s) => Ok(s),
        None => {
            tracing::warn!(
                synthesis_id = %id,
                requested_skill = %name,
                "unknown skill, falling back to baseline",
            );
            Ok(episcience_core::synthesis::skills::default_skill())
        }
    }
}
```

- [ ] **Step 4: Inject into the job pipeline**

In the existing `JobHandler` impl, find where `SynthesisPipeline::new(...)` is called. Add the resolution + `.with_skill(...)` step. Concretely, in the body of `handle`:

```rust
let skill = resolve_skill_for_row(&self.pool, payload.synthesis_id).await?;
let pipeline = SynthesisPipeline::new(
    self.pool.clone(),
    self.embedder.clone(),
    ArcLlm(self.llm.clone()),
    ArcEdgeProvider(self.edge_provider.clone()),
    query_embedding,
    self.cost_budget,
)
.with_skill(skill);
```

- [ ] **Step 5: Re-run the test**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-api jobs::synthesis_job
```

Expected: PASS (along with the existing job tests).

- [ ] **Step 6: Commit**

```bash
git add crates/episcience-api/src/jobs/synthesis_job.rs
git commit -m "feat(synthesis): inject row-specific skill into pipeline

Resolve syntheses.skill_name in the job handler; fall back to baseline
on an unknown name with a logged warning."
```

### Task 2.4 — Accept `skill_name` on the HTTP create route

**Files:**
- Modify: `crates/episcience-api/src/routes/syntheses.rs`

- [ ] **Step 1: Write the failing test**

In the route's test module:

```rust
#[tokio::test]
async fn post_syntheses_accepts_skill_name() {
    let app = test_app().await;
    let body = serde_json::json!({
        "query": "test",
        "skill_name": "baseline",
        "visibility": "private",
    });
    let resp = app.post("/api/v1/eln/syntheses", &body).await;
    assert_eq!(resp.status(), 202);
    let id: Uuid = resp.json::<serde_json::Value>()["id"]
        .as_str().unwrap().parse().unwrap();

    // Verify the row carries the skill we asked for.
    let stored: String = sqlx::query_scalar(
        "SELECT skill_name FROM syntheses WHERE id = $1"
    )
    .bind(id)
    .fetch_one(&app.pool)
    .await.unwrap();
    assert_eq!(stored, "baseline");
}

#[tokio::test]
async fn post_syntheses_omitted_skill_defaults_to_baseline() {
    let app = test_app().await;
    let body = serde_json::json!({ "query": "test", "visibility": "private" });
    let resp = app.post("/api/v1/eln/syntheses", &body).await;
    assert_eq!(resp.status(), 202);
    let id: Uuid = resp.json::<serde_json::Value>()["id"]
        .as_str().unwrap().parse().unwrap();
    let stored: String = sqlx::query_scalar(
        "SELECT skill_name FROM syntheses WHERE id = $1"
    )
    .bind(id)
    .fetch_one(&app.pool)
    .await.unwrap();
    assert_eq!(stored, "baseline");
}
```

- [ ] **Step 2: Run to confirm both fail**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-api routes::syntheses
```

Expected: FAIL (deserialisation rejects `skill_name`, or stored value mismatches).

- [ ] **Step 3: Extend the request struct + handler**

In `syntheses.rs` find the `CreateSynthesisRequest` (the struct deserialised from the request body). Add:

```rust
    #[serde(default)]
    pub skill_name: Option<String>,
```

In the handler, pass the value into the insert. If the existing insert is a single `sqlx::query!(...)`, extend it to set `skill_name`:

```rust
let skill_name = req.skill_name.as_deref().unwrap_or("baseline");
sqlx::query!(
    "INSERT INTO syntheses (id, query, agent_id, parent_synthesis_id,
        prereq_synthesis_ids, visibility, status, skill_name)
     VALUES ($1, $2, $3, $4, $5, $6, 'pending', $7)",
    id, req.query, auth.agent_id, req.parent_synthesis_id,
    &req.prereq_synthesis_ids, req.visibility.as_str(), skill_name,
)
.execute(&state.pool)
.await?;
```

- [ ] **Step 4: Re-run the tests**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-api routes::syntheses
```

Expected: 2 new tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/episcience-api/src/routes/syntheses.rs
git commit -m "feat(api): accept skill_name on POST /eln/syntheses

Defaults to 'baseline' when omitted. The job handler picks up the row's
skill_name and constructs the pipeline accordingly."
```

---

## Phase 3 — Move existing prompts behind `skill.section()` lookups

Goal: every place the pipeline builds a prompt today reads its section from `self.skill.section(stage)`. `BaselineSkill` returns the exact strings the pipeline used inline, so test output is byte-identical.

This phase is mechanical but easy to get wrong. Do it stage by stage, run tests after each edit, never bundle two stages into one commit.

### Task 3.1 — Stage 4 (Narration)

**Files:**
- Modify: `crates/episcience-db/src/synthesis/pipeline.rs`

- [ ] **Step 1: Find `build_narrate_prompt`**

It's called in `stage4_narrate` (around line 485). It currently embeds the narration prompt template inline.

- [ ] **Step 2: Pass the skill section in**

Change the signature of `build_narrate_prompt` to accept the section text:

```rust
fn build_narrate_prompt(
    skill_section: &str,
    c: &Cluster,
    contents: &[ClaimContent],
    subgraph_metadata: &serde_json::Value,
) -> String { ... }
```

Update the call site in `stage4_narrate`:

```rust
let section = self
    .skill
    .section(SynthesisStage::Narration)
    .unwrap_or("");
let prompt = build_narrate_prompt(section, c, &contents, &self.subgraph_metadata);
```

Inside the body of `build_narrate_prompt`, replace the previously-inline narration guidance with the `skill_section` argument. `BaselineSkill::section(SynthesisStage::Narration)` returns the same string the inline prompt carried, so the prompt is byte-identical.

- [ ] **Step 3: Run the existing stage-4 tests**

```bash
DATABASE_URL=postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    cargo test -p episcience-db synthesis::pipeline -- stage4
```

Expected: all stage-4 tests still PASS (no behaviour change).

- [ ] **Step 4: Add a regression test that proves the prompt now reads from the skill**

```rust
#[test]
fn narrate_prompt_includes_skill_section() {
    let cluster = Cluster::synthetic_singleton();   // existing helper
    let prompt = build_narrate_prompt(
        "INJECTED-SECTION-MARKER-12345",
        &cluster,
        &[],
        &serde_json::json!({}),
    );
    assert!(prompt.contains("INJECTED-SECTION-MARKER-12345"));
}
```

- [ ] **Step 5: Run it to verify the marker propagates**

```bash
cargo test -p episcience-db synthesis::pipeline::tests::narrate_prompt_includes_skill_section
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/episcience-db/src/synthesis/pipeline.rs
git commit -m "refactor(synthesis): stage4 narration reads skill section

BaselineSkill returns the previously-inline string, so behaviour is
unchanged. Custom skills can now override the narration guidance."
```

### Task 3.2 — Stage 5 (Composition)

Repeat the Task 3.1 pattern for `build_compose_prompt` (around line 555 of `pipeline.rs`).

- [ ] **Step 1: Pass `skill.section(SynthesisStage::Composition)` into `build_compose_prompt`**
- [ ] **Step 2: Add the marker regression test**
- [ ] **Step 3: Run stage-5 tests, confirm green**
- [ ] **Step 4: Commit:** `refactor(synthesis): stage5 composition reads skill section`

### Task 3.3 — Stage 2 (Traversal config from skill)

The skill's `traversal_config()` returns `Option<TraversalConfig>`. The job handler currently passes `payload.traversal_config` (with a defaults fallback). Add a precedence: explicit payload > skill override > defaults.

- [ ] **Step 1: Failing test in job handler tests:**

```rust
#[tokio::test]
async fn traversal_config_precedence_payload_then_skill_then_default() {
    // 1. Payload supplied -> wins
    // 2. Payload None + skill returns Some -> skill wins
    // 3. Both None -> defaults
    // (Construct one synthesis row per case, run resolution, assert
    // the final config.)
}
```

- [ ] **Step 2: Add `resolve_traversal_config` helper next to `resolve_skill_for_row`:**

```rust
pub fn resolve_traversal_config(
    payload_cfg: Option<&serde_json::Value>,
    skill: &dyn SynthesisSkill,
) -> TraversalConfig {
    if let Some(json) = payload_cfg {
        if let Ok(cfg) = serde_json::from_value::<TraversalConfig>(json.clone()) {
            return cfg;
        }
    }
    if let Some(cfg) = skill.traversal_config() {
        return cfg;
    }
    TraversalConfig::default()
}
```

- [ ] **Step 3: Use it in the job handler before stage2_traverse.**
- [ ] **Step 4: Run tests + commit:** `feat(synthesis): traversal config precedence — payload > skill > default`

---

## Phase 4 — Verifier-driven acceptance (Stage 6)

Goal: a new pipeline stage runs the skill's verifier after composition; on accept, the row moves to `complete`; on reject, the worker either refines (Phase 7) or moves to `failed` with the verifier reason recorded.

### Task 4.1 — `VerificationOutcome` type

**Files:**
- Create: `crates/episcience-core/src/synthesis/verifier.rs`

- [ ] **Step 1: Write the types**

```rust
//! Verifier types used by Stage 6.

use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationOutcome {
    Accept {
        rubric: String,
        evidence: serde_json::Value,
    },
    Reject {
        rubric: String,
        reason: VerificationReason,
        evidence: serde_json::Value,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationReason {
    /// A claim in the cluster was not cited in the narrative.
    UncitedMember { claim_id: Uuid },
    /// A citation referred to a claim outside the cluster.
    HallucinatedCitation { claim_id: Uuid },
    /// The narrative contradicts a kernel claim it should respect.
    KernelContradiction { claim_id: Uuid },
    /// Skill-specific veto.
    SkillRejection { detail: String },
}

#[derive(Debug)]
pub struct VerificationContext<'a> {
    pub synthesis_id: Uuid,
    pub query: &'a str,
    pub narrative: &'a str,
    pub cluster_member_ids: &'a [Uuid],
}
```

- [ ] **Step 2: Extend `SynthesisSkill` with a `verify()` default method**

In `skill.rs`:

```rust
use crate::synthesis::verifier::{VerificationContext, VerificationOutcome};

#[async_trait::async_trait]
pub trait SynthesisSkill: Send + Sync + std::fmt::Debug {
    // ... existing methods ...

    /// Verify a generated narrative against the cluster and the kernel
    /// state. The default impl runs the citation-discipline rubric: every
    /// member is cited, no citation hallucinates.
    async fn verify(
        &self,
        ctx: &VerificationContext<'_>,
    ) -> VerificationOutcome {
        crate::synthesis::verifier::default_citation_rubric(ctx)
    }
}
```

- [ ] **Step 3: Implement `default_citation_rubric` in `verifier.rs`**

```rust
pub fn default_citation_rubric(ctx: &VerificationContext<'_>) -> VerificationOutcome {
    let cite_re = regex::Regex::new(r"\[([0-9a-f-]{36})\]").expect("static");
    let cited: std::collections::HashSet<Uuid> = cite_re
        .captures_iter(ctx.narrative)
        .filter_map(|c| c[1].parse().ok())
        .collect();

    // 1. Every member must be cited.
    for m in ctx.cluster_member_ids {
        if !cited.contains(m) {
            return VerificationOutcome::Reject {
                rubric: "default_citation".into(),
                reason: VerificationReason::UncitedMember { claim_id: *m },
                evidence: serde_json::json!({ "cited": cited.iter().collect::<Vec<_>>() }),
            };
        }
    }

    // 2. No citation may be outside the cluster.
    let members: std::collections::HashSet<Uuid> =
        ctx.cluster_member_ids.iter().copied().collect();
    for c in &cited {
        if !members.contains(c) {
            return VerificationOutcome::Reject {
                rubric: "default_citation".into(),
                reason: VerificationReason::HallucinatedCitation { claim_id: *c },
                evidence: serde_json::json!({}),
            };
        }
    }

    VerificationOutcome::Accept {
        rubric: "default_citation".into(),
        evidence: serde_json::json!({ "cited_count": cited.len() }),
    }
}
```

- [ ] **Step 4: Test the rubric**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn accepts_when_every_member_is_cited_and_no_hallucinations() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let narrative = format!("Saw [{a}] and [{b}].");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a, b],
        };
        match default_citation_rubric(&ctx) {
            VerificationOutcome::Accept { .. } => {}
            other => panic!("expected accept, got {other:?}"),
        }
    }

    #[test]
    fn rejects_when_member_is_uncited() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let narrative = format!("Only saw [{a}].");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a, b],
        };
        match default_citation_rubric(&ctx) {
            VerificationOutcome::Reject {
                reason: VerificationReason::UncitedMember { claim_id }, ..
            } => assert_eq!(claim_id, b),
            other => panic!("expected uncited-member reject, got {other:?}"),
        }
    }

    #[test]
    fn rejects_when_citation_is_hallucinated() {
        let a = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let narrative = format!("Saw [{a}] and [{intruder}].");
        let ctx = VerificationContext {
            synthesis_id: Uuid::new_v4(),
            query: "q",
            narrative: &narrative,
            cluster_member_ids: &[a],
        };
        match default_citation_rubric(&ctx) {
            VerificationOutcome::Reject {
                reason: VerificationReason::HallucinatedCitation { claim_id }, ..
            } => assert_eq!(claim_id, intruder),
            other => panic!("expected hallucinated-citation reject, got {other:?}"),
        }
    }
}
```

- [ ] **Step 5: Run tests + commit**

```bash
cargo test -p episcience-core synthesis::verifier
git add crates/episcience-core/src/synthesis/verifier.rs \
        crates/episcience-core/src/synthesis/skill.rs
git commit -m "feat(synthesis): verification rubric + default citation check"
```

### Task 4.2 — Persist verifier outcome on the row

**Files:**
- Create: `migrations/5019b_syntheses_verifier_outcome.sql`
- Modify: `crates/episcience-db/src/synthesis/pipeline.rs` (Stage 6)
- Modify: `crates/episcience-api/src/jobs/synthesis_job.rs`

- [ ] **Step 1: Write the migration**

```sql
-- 5019b_syntheses_verifier_outcome.sql
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS verifier_outcome JSONB,
    ADD COLUMN IF NOT EXISTS verifier_attempts SMALLINT NOT NULL DEFAULT 0;

-- Verifier-rejected syntheses end up in a new lifecycle state.
ALTER TABLE syntheses
    DROP CONSTRAINT IF EXISTS syntheses_status_check;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_status_check
    CHECK (status IN (
        'pending', 'running', 'verifying',
        'complete', 'failed', 'deleted', 'rejected'
    ));
```

- [ ] **Step 2: Apply + verify**

```bash
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -f migrations/5019b_syntheses_verifier_outcome.sql
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -c "\d syntheses" | grep -E 'verifier|status'
```

- [ ] **Step 3: Add `stage6_verify` to the pipeline**

In `pipeline.rs` (after `stage5_compose`):

```rust
    /// Stage 6 — Verify.
    ///
    /// Runs the skill's verifier against the composed narrative. The
    /// outcome is returned so the caller can decide whether to advance
    /// to `complete`, refine (Phase 7), or mark `rejected`.
    pub async fn stage6_verify(
        &self,
        synthesis_id: Uuid,
        query: &str,
        narrative: &str,
        cluster_member_ids: &[Uuid],
    ) -> Result<VerificationOutcome, SynthesisError> {
        let ctx = VerificationContext {
            synthesis_id,
            query,
            narrative,
            cluster_member_ids,
        };
        Ok(self.skill.verify(&ctx).await)
    }
```

Add the import at the top of `pipeline.rs`:

```rust
use episcience_core::synthesis::verifier::{VerificationContext, VerificationOutcome};
```

- [ ] **Step 4: Job handler routes on outcome**

In `synthesis_job.rs::handle`, after composition succeeds:

```rust
let cluster_member_ids: Vec<Uuid> = clusters.iter()
    .flat_map(|c| c.member_claim_ids.iter().copied())
    .collect();

// Transition: running -> verifying
mark_status(&self.pool, payload.synthesis_id, "verifying").await?;

let outcome = pipeline
    .stage6_verify(payload.synthesis_id, &payload.query, &narrative, &cluster_member_ids)
    .await?;

sqlx::query!(
    "UPDATE syntheses
        SET verifier_outcome = $2,
            verifier_attempts = verifier_attempts + 1
      WHERE id = $1",
    payload.synthesis_id,
    serde_json::to_value(&outcome).unwrap(),
)
.execute(&self.pool)
.await
.map_err(|e| JobError::Db(e.to_string()))?;

match outcome {
    VerificationOutcome::Accept { .. } => {
        finalise_complete(&self.pool, payload.synthesis_id, &narrative).await?;
    }
    VerificationOutcome::Reject { .. } => {
        // Phase 7 will refine here. For now, mark rejected.
        mark_status(&self.pool, payload.synthesis_id, "rejected").await?;
    }
}
```

- [ ] **Step 5: Add an end-to-end test**

```rust
#[tokio::test]
async fn synthesis_with_uncited_member_is_rejected() {
    let app = test_app_with_mock_llm_returning(/* narrative omitting one member */).await;
    let id = app.post_synthesis_and_wait("test", "baseline").await;
    let row = app.fetch_synthesis(id).await;
    assert_eq!(row.status, "rejected");
    let outcome: VerificationOutcome =
        serde_json::from_value(row.verifier_outcome.unwrap()).unwrap();
    matches!(outcome, VerificationOutcome::Reject { .. });
}
```

- [ ] **Step 6: Run + commit**

```bash
cargo test -p episcience-api jobs::synthesis_job
git add migrations/5019b_syntheses_verifier_outcome.sql \
        crates/episcience-db/src/synthesis/pipeline.rs \
        crates/episcience-api/src/jobs/synthesis_job.rs
git commit -m "feat(synthesis): stage6 verifier gates status=complete

Rejected syntheses move to status='rejected' with the outcome persisted
in verifier_outcome. Phase 7 will route reject -> refine instead of
straight to rejected."
```

---

## Phase 5 — Second skill: `LabNotebookSkill`

Goal: prove the skill contract is real by adding a second skill that overrides Narration, Composition, and Traversal.

### Task 5.1 — Implement `LabNotebookSkill`

**Files:**
- Create: `crates/episcience-core/src/synthesis/skills/lab_notebook.rs`
- Modify: `crates/episcience-core/src/synthesis/skills/mod.rs`
- Create: `crates/episcience-core/src/synthesis/skills/markdown/lab_notebook.md`
- Create: `migrations/5019c_syntheses_skill_lab_notebook.sql`

- [ ] **Step 1: Failing tests for the skill**

```rust
// in lab_notebook.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};

    #[test]
    fn lab_notebook_overrides_narration_composition_traversal() {
        let s = LabNotebookSkill;
        assert_eq!(s.name(), "lab_notebook");
        let narration = s.section(SynthesisStage::Narration).unwrap();
        assert!(narration.to_lowercase().contains("protocol"));
        assert!(narration.to_lowercase().contains("sample"));
        let cfg = s.traversal_config().expect("lab_notebook sets traversal");
        assert_eq!(cfg.max_depth, 2);
    }
}
```

- [ ] **Step 2: Implementation**

```rust
//! `LabNotebookSkill` — synthesis tuned for ELN narrative summaries.
//!
//! Differs from baseline in three ways:
//! - prefers traversal depth 2 with `derived_from` + `observation` edges
//! - narration cites protocols and samples by id alongside claims
//! - composition produces a chronological narrative not a thematic one

use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};
use crate::synthesis::traversal::{EdgeType, TraversalConfig};

#[derive(Debug, Default)]
pub struct LabNotebookSkill;

#[async_trait::async_trait]
impl SynthesisSkill for LabNotebookSkill {
    fn name(&self) -> &'static str { "lab_notebook" }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Narration =>
                "For each cluster, write a chronological 2-4 sentence \
                 summary mentioning the protocol used and the samples \
                 observed. Cite every claim with `[<claim_id>]`. Cite \
                 protocols as `(protocol:<title>@v<version>)` and samples \
                 as `(sample:<name>)` when relevant. Do not invent any.",
            SynthesisStage::Composition =>
                "Compose the per-cluster summaries into a chronologically \
                 ordered Markdown narrative (oldest first). Keep the \
                 `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.",
            _ => return None,
        })
    }

    fn traversal_config(&self) -> Option<TraversalConfig> {
        Some(TraversalConfig {
            max_depth: 2,
            edge_types: vec![EdgeType::DerivedFrom, EdgeType::Observation],
            relevance_prune: 0.55,
            ..TraversalConfig::default()
        })
    }
}
```

- [ ] **Step 3: Register in `skills/mod.rs`**

Add `pub mod lab_notebook;` and extend `load_by_name`:

```rust
        "lab_notebook" => Some(Arc::new(lab_notebook::LabNotebookSkill)),
```

- [ ] **Step 4: Migration extends the CHECK list**

```sql
-- 5019c_syntheses_skill_lab_notebook.sql
ALTER TABLE syntheses
    DROP CONSTRAINT syntheses_skill_name_known;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline', 'lab_notebook'));
```

- [ ] **Step 5: Markdown reference**

Create `crates/episcience-core/src/synthesis/skills/markdown/lab_notebook.md` mirroring the Rust strings, plus an `Overview` section describing the domain.

- [ ] **Step 6: End-to-end test that the skill is honoured**

```rust
#[tokio::test]
async fn lab_notebook_skill_is_loaded_when_named() {
    let app = test_app().await;
    let body = serde_json::json!({
        "query": "what happened in batch 12?",
        "skill_name": "lab_notebook",
        "visibility": "private",
    });
    let resp = app.post("/api/v1/eln/syntheses", &body).await;
    assert_eq!(resp.status(), 202);
    // The job handler resolves and calls .with_skill — assert via a
    // side-effect we can observe (e.g. capture the skill name from a
    // mock that records it).
}
```

- [ ] **Step 7: Run + commit**

```bash
psql postgres://epigraph:epigraph@127.0.0.1:5432/epigraph_db_repo_test \
    -f migrations/5019c_syntheses_skill_lab_notebook.sql
cargo test -p episcience-core synthesis::skills::lab_notebook
cargo test --workspace --lib --bins
git add -A
git commit -m "feat(synthesis): add LabNotebookSkill specialisation

Overrides narration, composition, and traversal config for ELN-style
chronological synthesis."
```

---

## Phase 6 — Novelty assessment stage

Goal: a Stage 7 (`stage7_novelty`) that scores the synthesis against prior syntheses by membership-overlap + narrative cosine distance, optionally consulting an external `NoveltyBackend`. Score is persisted on the row.

### Task 6.1 — `NoveltyScore` + `NoveltyBackend`

**Files:**
- Create: `crates/episcience-core/src/synthesis/novelty.rs`
- Create: `migrations/5020_syntheses_novelty.sql`

- [ ] **Step 1: Types**

```rust
//! Novelty assessment — Stage 7.

use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NoveltyScore {
    /// 0.0 (fully redundant) to 1.0 (highly novel). Computed.
    pub score: f64,
    /// The backend that produced the score.
    pub backend: String,
    /// Prior syntheses that overlap, sorted descending by similarity.
    pub neighbours: Vec<NoveltyNeighbour>,
    /// Free-form rationale text from the backend.
    pub rationale: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct NoveltyNeighbour {
    pub synthesis_id: Uuid,
    pub similarity: f64,
    /// Fraction of cluster members shared with the candidate.
    pub member_overlap: f64,
}

#[async_trait::async_trait]
pub trait NoveltyBackend: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &'static str;

    async fn score(
        &self,
        candidate_synthesis_id: Uuid,
        candidate_narrative: &str,
        candidate_member_ids: &[Uuid],
    ) -> Result<NoveltyScore, NoveltyError>;
}

#[derive(Debug, thiserror::Error)]
pub enum NoveltyError {
    #[error("db: {0}")]
    Db(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}
```

- [ ] **Step 2: Migration**

```sql
-- 5020_syntheses_novelty.sql
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS novelty_score JSONB,
    ADD COLUMN IF NOT EXISTS novelty_backend TEXT;
```

- [ ] **Step 3: Default backend — prior-syntheses similarity**

Create `crates/episcience-db/src/synthesis/novelty_backend_internal.rs`:

```rust
//! Internal novelty backend — scores against prior `syntheses` rows.
//!
//! Algorithm: for each prior `complete` synthesis with overlapping cluster
//! members, compute (member_overlap, narrative_cosine). Aggregate to a
//! single score = 1.0 - max(0.5 * member_overlap + 0.5 * narrative_cosine).

use crate::synthesis::SynthesisRepository;
use episcience_core::synthesis::novelty::{
    NoveltyBackend, NoveltyError, NoveltyNeighbour, NoveltyScore,
};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug)]
pub struct InternalNoveltyBackend {
    pub pool: PgPool,
    pub embedder: Arc<dyn epigraph_embeddings::EmbeddingService>,
}

#[async_trait::async_trait]
impl NoveltyBackend for InternalNoveltyBackend {
    fn name(&self) -> &'static str { "internal_prior_syntheses" }

    async fn score(
        &self,
        candidate_id: Uuid,
        candidate_narrative: &str,
        candidate_member_ids: &[Uuid],
    ) -> Result<NoveltyScore, NoveltyError> {
        // 1. Find prior syntheses sharing any member.
        let prior = SynthesisRepository::priors_with_overlap(
            &self.pool, candidate_id, candidate_member_ids,
        )
        .await
        .map_err(|e| NoveltyError::Db(e.to_string()))?;

        if prior.is_empty() {
            return Ok(NoveltyScore {
                score: 1.0,
                backend: self.name().to_string(),
                neighbours: vec![],
                rationale: "no prior synthesis shares any cluster member".into(),
            });
        }

        // 2. Embed candidate narrative once.
        let cand_emb = self.embedder.generate_query(candidate_narrative)
            .await
            .map_err(|e| NoveltyError::Unavailable(e.to_string()))?;

        // 3. Score each prior, keep top 5.
        let mut scored: Vec<NoveltyNeighbour> = prior.into_iter()
            .map(|p| {
                let cos = cosine(&cand_emb, &p.narrative_embedding);
                let overlap = jaccard(&p.member_ids, candidate_member_ids);
                NoveltyNeighbour {
                    synthesis_id: p.id,
                    similarity: 0.5 * cos + 0.5 * overlap,
                    member_overlap: overlap,
                }
            })
            .collect();
        scored.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        scored.truncate(5);

        let top = scored.first().map(|n| n.similarity).unwrap_or(0.0);
        Ok(NoveltyScore {
            score: (1.0 - top).clamp(0.0, 1.0),
            backend: self.name().to_string(),
            neighbours: scored,
            rationale: format!("top-prior similarity {top:.3}"),
        })
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

fn jaccard(a: &[Uuid], b: &[Uuid]) -> f64 {
    use std::collections::HashSet;
    let sa: HashSet<&Uuid> = a.iter().collect();
    let sb: HashSet<&Uuid> = b.iter().collect();
    let union = sa.union(&sb).count();
    if union == 0 { 0.0 } else { sa.intersection(&sb).count() as f64 / union as f64 }
}
```

Unit tests for the helpers (same file, `#[cfg(test)] mod helper_tests`):

```rust
#[test]
fn cosine_orthogonal_is_zero() {
    assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
}
#[test]
fn cosine_identical_is_one() {
    assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-9);
}
#[test]
fn cosine_empty_is_zero_not_nan() {
    assert_eq!(cosine(&[], &[]), 0.0);
}
#[test]
fn jaccard_disjoint_is_zero() {
    let a = vec![Uuid::new_v4()];
    let b = vec![Uuid::new_v4()];
    assert_eq!(jaccard(&a, &b), 0.0);
}
#[test]
fn jaccard_identical_is_one() {
    let u = Uuid::new_v4();
    assert_eq!(jaccard(&[u], &[u]), 1.0);
}
#[test]
fn jaccard_half_overlap() {
    let shared = Uuid::new_v4();
    let a = vec![shared, Uuid::new_v4()];
    let b = vec![shared, Uuid::new_v4()];
    // intersection 1, union 3 → 1/3
    assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
}
```

- [ ] **Step 4: Stage 7 in pipeline**

```rust
    pub async fn stage7_novelty(
        &self,
        synthesis_id: Uuid,
        narrative: &str,
        cluster_member_ids: &[Uuid],
        backend: &dyn NoveltyBackend,
    ) -> Result<NoveltyScore, SynthesisError> {
        backend
            .score(synthesis_id, narrative, cluster_member_ids)
            .await
            .map_err(|e| SynthesisError::Db(e.to_string()))
    }
```

- [ ] **Step 5: Job handler invokes Stage 7 after accept**

```rust
if let VerificationOutcome::Accept { .. } = outcome {
    let backend = InternalNoveltyBackend {
        pool: self.pool.clone(),
        embedder: self.embedder.clone(),
    };
    let novelty = pipeline.stage7_novelty(
        payload.synthesis_id, &narrative, &cluster_member_ids, &backend,
    ).await?;
    sqlx::query!(
        "UPDATE syntheses SET novelty_score = $2, novelty_backend = $3 WHERE id = $1",
        payload.synthesis_id,
        serde_json::to_value(&novelty).unwrap(),
        novelty.backend,
    )
    .execute(&self.pool).await.map_err(|e| JobError::Db(e.to_string()))?;
    finalise_complete(&self.pool, payload.synthesis_id, &narrative).await?;
}
```

- [ ] **Step 6: Tests**

Three end-to-end tests, each using a deterministic `MockEmbeddingService` that returns a fixed vector per input string so cosine values are reproducible.

```rust
#[tokio::test]
async fn novelty_is_one_when_no_priors() {
    let pool = test_pool().await;
    let embedder = Arc::new(deterministic_embedder());
    let backend = InternalNoveltyBackend { pool: pool.clone(), embedder };
    let cand_id = Uuid::new_v4();
    let members = vec![Uuid::new_v4(), Uuid::new_v4()];
    let s = backend.score(cand_id, "a novel summary", &members).await.unwrap();
    assert_eq!(s.score, 1.0);
    assert!(s.neighbours.is_empty());
}

#[tokio::test]
async fn novelty_is_low_when_prior_has_same_members_and_text() {
    let pool = test_pool().await;
    let embedder = Arc::new(deterministic_embedder());
    let prior_members = vec![Uuid::new_v4(), Uuid::new_v4()];
    insert_complete_synthesis(
        &pool, "shared narrative", &prior_members, &embedder,
    ).await;
    let backend = InternalNoveltyBackend { pool, embedder };
    let s = backend.score(Uuid::new_v4(), "shared narrative", &prior_members).await.unwrap();
    assert!(s.score < 0.1, "expected near-zero novelty, got {}", s.score);
    assert_eq!(s.neighbours.len(), 1);
}

#[tokio::test]
async fn novelty_is_mid_when_prior_has_half_member_overlap_and_different_text() {
    let pool = test_pool().await;
    let embedder = Arc::new(deterministic_embedder());
    let shared = Uuid::new_v4();
    let prior_members = vec![shared, Uuid::new_v4()];
    let cand_members = vec![shared, Uuid::new_v4()];
    insert_complete_synthesis(&pool, "prior text alpha", &prior_members, &embedder).await;
    let backend = InternalNoveltyBackend { pool, embedder };
    let s = backend.score(Uuid::new_v4(), "candidate text omega", &cand_members).await.unwrap();
    // Cosine of two unrelated short strings under the deterministic embedder
    // should be close to 0; jaccard = 1/3. Top neighbour similarity ≈ 0.5*0
    // + 0.5*(1/3) ≈ 0.167; score ≈ 1 - 0.167 ≈ 0.833.
    assert!(s.score > 0.5 && s.score < 1.0, "expected mid novelty, got {}", s.score);
    assert_eq!(s.neighbours.len(), 1);
    assert!((s.neighbours[0].member_overlap - 1.0 / 3.0).abs() < 1e-9);
}
```

Helpers `deterministic_embedder()` and `insert_complete_synthesis(...)` go in the same test module; the first returns embeddings derived from a stable hash of the input string, the second writes a `syntheses` row in `complete` status plus the membership rows.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "feat(synthesis): stage7 novelty scoring against prior syntheses

Default backend computes 0.5 * cosine + 0.5 * jaccard against prior
syntheses sharing cluster members. Score persisted on syntheses row."
```

---

## Phase 7 — Simulated-annealing refinement via `REFINES`

Goal: when Stage 6 rejects, the worker creates a refinement child (a new `syntheses` row pointing back via `REFINES`) with a thawed `RefinementTemperature` that widens traversal config and (optionally) loosens the verifier rubric. The chain is bounded.

### Task 7.1 — `RefinementTemperature` type

**Files:**
- Create: `crates/episcience-core/src/synthesis/refinement.rs`
- Create: `migrations/5021_syntheses_refinement_temperature.sql`

- [ ] **Step 1: Type + anneal function**

```rust
//! Refinement temperature. SciLink's "simulated-annealing agentic pipelines"
//! hold priors strict at first, then progressively thaw as iterations fail.

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RefinementTemperature {
    /// Depth steps over the skill's default traversal config.
    pub depth_delta: u8,
    /// Multiplier on relevance_prune (>1 keeps more neighbours).
    pub relevance_prune_relax: f32,
    /// True after the first reject — verifier may downgrade strict rubrics.
    pub allow_soft_verifier: bool,
}

impl Default for RefinementTemperature {
    fn default() -> Self {
        Self { depth_delta: 0, relevance_prune_relax: 1.0, allow_soft_verifier: false }
    }
}

impl RefinementTemperature {
    /// Anneal one step. Bounded by a hard ceiling (depth_delta <= 3).
    pub fn anneal(self) -> Self {
        Self {
            depth_delta: self.depth_delta.saturating_add(1).min(3),
            relevance_prune_relax: (self.relevance_prune_relax * 0.8).max(0.4),
            allow_soft_verifier: true,
        }
    }
}
```

- [ ] **Step 2: Migration**

```sql
-- 5021_syntheses_refinement_temperature.sql
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS refinement_temperature JSONB
    DEFAULT '{"depth_delta":0,"relevance_prune_relax":1.0,"allow_soft_verifier":false}'::jsonb;
```

- [ ] **Step 3: Refine-on-reject in the job handler**

```rust
const MAX_REFINEMENTS: u8 = 3;

VerificationOutcome::Reject { .. } => {
    let temp = fetch_temperature(&self.pool, payload.synthesis_id).await?;
    if temp.depth_delta >= MAX_REFINEMENTS {
        mark_status(&self.pool, payload.synthesis_id, "rejected").await?;
        return Ok(...);
    }
    let new_temp = temp.anneal();
    let child = create_refinement_child(
        &self.pool, payload.synthesis_id, &payload.query, new_temp,
    ).await?;
    enqueue_synthesis_job(&self.pool, child, &payload.query, new_temp).await?;
    // Original synthesis stays in 'rejected' — the child is the next attempt.
}
```

`create_refinement_child` writes a new `syntheses` row + a `synthesis_provo_edges` row with predicate `REFINES`.

- [ ] **Step 4: Tests**

Test the chain: reject -> create child with depth_delta=1 -> reject -> depth_delta=2 -> reject -> depth_delta=3 -> stop. Assert: at depth_delta=3 the chain terminates with status='rejected' on the leaf and no further child is created.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(synthesis): simulated-annealing refinement on verifier reject

Up to MAX_REFINEMENTS=3 children spawn, each with a thawed
RefinementTemperature (widens traversal, loosens verifier strictness).
PROV-O REFINES edges record the chain."
```

---

## Phase 8 — MCP surface parity

Goal: every HTTP write path that an agent needs has a corresponding MCP tool. The existing MCP server exposes `synthesize` + `recall_synthesis`; add four more to match SciLink's surface.

### Task 8.1 — New MCP tools

**Files:**
- Create: `crates/episcience-api/src/mcp/eln_writes.rs`
- Modify: `crates/episcience-api/src/mcp/mod.rs`

Tools to add:

1. `propose_protocol(title, steps, equipment, supersedes?, labels?, properties?) -> Protocol`
2. `add_observation(sample_id, content, relationship?) -> { claim_id, sample_id, relationship }`
3. `countersign(claim_id, signature_meaning, signature_hex, public_key_hex) -> { id, ... }`
4. `attach_blob(file_bytes_base64, filename, mime_type, sample_id?, labels?, properties?) -> Blob`

For each tool:

- [ ] **Step 1: Write the failing test** that calls the MCP tool through the in-process client and asserts the DB side-effect matches what the HTTP route would have produced.
- [ ] **Step 2: Implement** as a thin wrapper that calls the same `routes::*` handler internals (extract the body of the HTTP handler into a private function the MCP tool also calls — DRY).
- [ ] **Step 3: Run + commit individually:** `feat(mcp): add <tool_name> MCP tool`.

### Task 8.2 — Integration test from Claude Code's perspective

- [ ] Use the in-process MCP test harness to run a full ELN turn: propose protocol → create sample → add observation → attach blob → synthesize → countersign. Assert every row is present, the claim has at least one countersignature with `signature_meaning = approved`, and the synthesis is `complete`.

- [ ] Commit: `test(mcp): end-to-end ELN turn through MCP only`.

---

## Phase 9 — Protocol section vocabulary (additive)

Goal: protocols gain structured sections (`overview`, `planning`, `implementation`, `interpretation`, `validation`) without breaking the existing `steps + equipment + safety_notes` shape.

### Task 9.1 — Migration

**Files:**
- Create: `migrations/5022_protocols_section_vocabulary.sql`

```sql
-- 5022_protocols_section_vocabulary.sql
ALTER TABLE protocols
    ADD COLUMN IF NOT EXISTS sections JSONB
    DEFAULT '{}'::jsonb;
-- Sections is a small dict { "overview": "...", "planning": "...", ... }.
-- Off-vocabulary keys are preserved verbatim and surfaced under "extras"
-- by the API layer (mirrors SciLink's loader warning behaviour).
```

### Task 9.2 — Core model + route

**Files:**
- Modify: `crates/episcience-core/src/protocol.rs`
- Modify: `crates/episcience-api/src/routes/protocols.rs`

- [ ] **Step 1: Add `ProtocolSections` struct with five named fields + an `extras: HashMap<String, String>`** for off-vocab content.
- [ ] **Step 2: Extend `CreateProtocolRequest` with `sections: Option<ProtocolSections>`** (defaulting empty).
- [ ] **Step 3: Validate off-vocab keys** in the handler — emit a structured warning header (`X-Episcience-Protocol-Warnings: extras_dropped=foo,bar`) and persist them under `sections.extras`.
- [ ] **Step 4: Tests:** create a protocol with valid sections; create with off-vocab key; verify warning header + persisted extras.
- [ ] **Step 5: Commit:** `feat(protocols): structured section vocabulary (additive)`.

---

## Phase 10 — Documentation + finishing

### Task 10.1 — Docs updates

**Files:**
- Modify: `docs/intro/02-concepts-science.md` — add a §7 on synthesis skills, a §8 on verifier outcomes, a §9 on novelty assessment, a §10 on refinement chains.
- Modify: `README.md` — add a one-paragraph blurb pointing at the new skill system.

### Task 10.2 — Self-review checklist

- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`. Fix any new warnings introduced by these phases.
- [ ] Run `cargo test --workspace`. Confirm all green and the test count is `baseline + new tests added across phases`.
- [ ] Run `sqlx migrate info` against the dev DB. Confirm migration ordering is monotonic with no holes.
- [ ] Use `superpowers:requesting-code-review` against the merged branch to confirm spec coverage matches this plan.

### Task 10.3 — PR

- [ ] Open the PR with body summarising the lesson disposition table (top of this plan) and which phases it lands. Title: `feat(synthesis): SciLink-inspired skill foundation + verifier + novelty`.

---

## Out of scope (separate plans)

### Lessons that belong in EpiClaw

**Lesson 4 — three autonomy levels (co-pilot / autopilot / autonomous):** EpiClaw is the agent orchestrator and is the right home for autonomy. The mapping into episcience is the `visibility` lifecycle gate — but only EpiClaw knows whether the user is present to approve. A separate EpiClaw plan should:

1. Add `agent_runner::AutonomyLevel { CoPilot, Autopilot, Autonomous }` matching SciLink's three-mode enum.
2. Map autonomy onto signing posture: AUTOPILOT requires a human-in-loop countersign call to episcience before MCP writes graduate from `private` to `shared`; AUTONOMOUS auto-signs with the agent's own key; CO_PILOT pauses for an out-of-band approval token.
3. Surface the autonomy level on the `claim.signature.context` so consumers can tell which mode produced a claim.

**Lesson 7 — meta-agent `run_task(task, context)` contract:** SciLink's meta-agent threads `key_findings` / `files_produced` between mode delegations. EpiClaw is the closest analogue (it already orchestrates agents). A separate EpiClaw plan should:

1. Define a stable Rust `EpiclawTask` enum (`Synthesize`, `Countersign`, `IngestBlob`, …) with structured `context` fields.
2. Implement a `run_task(task, context) -> TaskResult { summary, files_produced, key_findings, suggested_followups }` shape on the agent runner. The result schema is uniform across task kinds; the domain payload differs per kind.
3. Use it to chain ELN turns: an outer agent calls `run_task(IngestBlob)` then threads the resulting `blob_id` into `run_task(AddObservation)` and `run_task(Synthesize)`.

### Lessons evaluated for the EpiGraph kernel — none land there

The kernel is intentionally minimal. We considered:

- **Generic content-addressed `bundles` table** — could host skill bundles for any downstream app. Rejected as YAGNI: episcience is the only consumer today. Revisit when a second downstream app (EpiClaw, Praxis) wants the same shape.
- **First-class `verifier_outcome` on `claims`** — could unify episcience verification with future cross-app verifiers. Rejected for now: the verifier outcome semantics are application-specific (citation discipline, kernel-contradiction checks). Keep the abstraction in episcience until a second app demonstrates the same shape.
- **Section vocabulary as a kernel primitive** — definitively no. Section vocabularies are per-modality (analyze/plan/simulate in SciLink; synthesis/protocol in episcience). The kernel does not own modalities.

If a future plan needs to break either rejection, it should justify the second downstream consumer concretely — "we'd find this useful here" is not enough; "epiclaw and praxis both implement this same pattern by hand, and we want to share" is.
