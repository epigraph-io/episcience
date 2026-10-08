# Wiki Phase B — Articles in EpiScience: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Generate one theme-anchored, group-scoped, cited wiki article per (group, top-level theme) as an EpiScience synthesis, with a registry and read API that Phase C (Explorer `/wiki`) can render.

**Architecture:** A wiki article **is** a synthesis: `skill_name = 'wiki_article'`, `visibility = 'group'`, `owner_group_id = G`, plus two new nullable columns on `public.syntheses` (`seed_theme_id`, `wiki_key`). The registry is a read over `syntheses` (latest *complete* row per `(owner_group_id, wiki_key)`; older rows are the history), so it inherits the existing row security and tenancy guards with no new tenant table. Stage 1 gains a theme-anchored seed: viewer-filtered theme members from the kernel (`ClaimThemeRepository::claims_in_themes_at_dim`), then a pure MMR selection with near-duplicate suppression. The existing seed filter still enforces "public + owner group only".

**Tech Stack:** Rust (tokio, sqlx runtime queries — this repo uses no `query!` macros), rmcp tools, axum routes, PostgreSQL 16 + pgvector, the E1 test harness (`scripts/e1-test-db.sh`).

**Spec:** `docs/superpowers/plans/2026-10-07-epigraph-wiki.md` (phase B) + the Phase 0 spike result (private: `~/ops-private/wiki/phase0-spike-2026-10-07.md`; summary in "Spike evidence" below).

## Spike evidence (why the design is what it is)

Phase 0 (2026-10-07, prod, deployed Aug-2 build) — GO: 5/5 syntheses completed, 226/226 citations resolve, ~3–4 min and ~13 LLM calls each.
- **Text seeding drifts off-theme.** 206 of 210 themes have no scope note, so the only text handle is the label. A label-only seed cited 0 of its own theme's claims (S5: "Building Code Dimensional Requirements" pulled 37/42 from a larger neighbouring theme) and 42/50 (S4). ⇒ **theme-anchored seed is required** (Task 3).
- **Repetition.** Near-duplicate claims (same textbook fact across editions) are restated cluster after cluster. ⇒ **near-duplicate suppression at seed time** (Task 1) + a within-cluster narration instruction (Task 2). Cross-cluster dedup cannot happen at composition: `stage5_compose` requires every cluster summary verbatim inside its `<<<CLUSTER:…>>>` sentinels and rejects any edit (`ComposeAnchorViolation`, the one failure seen before the spike), so the composer may only write text *between* blocks.
- **No traversal in production.** `episcience-worker` uses `EmptyEdgeProvider` (`crates/episcience-api/src/bin/episcience-worker.rs`), so an article = its ≤50 seeds and there are no contradiction edges. ⇒ the "Contested" section is LLM-reported only (Task 2); edge-backed Contested waits for the real edge provider (B-CKL Phase 4). "See also" moves to Phase C (the Explorer can ask the kernel for related themes).
- **Coverage.** 50 seeds vs 1k–7k members. ⇒ seeds are chosen for diversity (MMR), not nearest-50.

## Decisions taken (operator, 2026-10-07)

- **Per-group curator agent.** Generation runs as an agent that is a member of exactly one group (group:main first), so its whole read scope equals the group's. Provisioning that agent is an operator action (runbook in Task 6); no code creates identities.
- Wiki pages are keyed on theme `properties` (`cluster_run_id`, `cluster_id`, `split_part`), never on theme UUIDs (re-projection mints new ids).

## Global Constraints

- Every migration from 5033 on obeys `migrations/README.md` "Rules for every migration from 5033 on" (checked by `crates/episcience-db/tests/migration_lint.rs`): first statement `SELECT public.episcience_assert_kernel_contract(1);`, second `SET LOCAL lock_timeout = '5s';`, every object `public.`-qualified, no DDL/DML/grant on a kernel table, no role statements.
- New migration number: **5041** (next free).
- No kernel-contract change: kernel objects are read only through the pinned kernel crates (`epigraph_db`, `epigraph_engine`) except the one runtime read of `claim_themes.properties` (Task 4), which is not a tenancy row (see `ClaimThemeRepository::get_summary` doc: "the theme row itself is not a tenancy row").
- **Production safety (every agent, every task):** never read `~/episcience/.env`, `/etc/episcience/*`, `/etc/epiclaw/*` or any unit file; never set or export `DATABASE_URL`, `MAINTENANCE_DATABASE_URL` or any `EPISCIENCE_*DATABASE_URL` yourself; `episcience-migrate` and every DB test run ONLY through `scripts/e1-test-db.sh` with `E1_TEST_ADMIN_URL=postgres://epigraph:epigraph@127.0.0.1:5433/postgres` (the throwaway test cluster; the script refuses port 5432). Prod Postgres is on the same host: touching it is out of scope.
- **One cargo at a time** on this 4-core / 7.6 GB host that also runs production; `CARGO_PROFILE_DEV_DEBUG=0` and `SQLX_OFFLINE=true` on every cargo invocation.
- Every DB read in a request path runs on the viewer's connection (`server.db.read_as(viewer)` / `write_as(viewer)`); `crates/episcience-api/tests/no_unscoped_pool.rs` fails otherwise.
- Constants (in `episcience_core::wiki`): `WIKI_SKILL_NAME = "wiki_article"`, `CANDIDATE_POOL = 400` (≤ kernel `MAX_CANDIDATE_POOL` 1000), `SEED_BUDGET = 50`, `DUP_COSINE = 0.95`, `MMR_LAMBDA = 0.7`, `MIN_READABLE_MEMBERS = 20`.
- Clippy 1.99 `double_must_use`: any new `#[async_trait]` **trait definition** gets `#[allow(clippy::double_must_use)] // async_trait's generated #[must_use] on an already-must-use boxed future` (impls do not).
- Commits follow the Epistemic Commit Protocol (`<type>(<scope>): <claim>` + Evidence / Reasoning / Verification).
- Environment for EVERY cargo / harness command (the harness finds `episcience-migrate` via `CARGO_TARGET_DIR`; without it the run fails "No such file or directory"):
  `export CARGO_TARGET_DIR=/home/jeremy/.cargo-target SQLX_OFFLINE=true CARGO_PROFILE_DEV_DEBUG=0 E1_TEST_ADMIN_URL=postgres://epigraph:epigraph@127.0.0.1:5433/postgres E1_KERNEL_TOOLS_DIR=$HOME/.cache/episcience-e1`
- Local gate (no GitHub CI waiting; mirrors `.github/workflows/ci.yml`): `scripts/e1-test-db-selftest.sh`; `cargo fmt --all -- --check`; `cargo +1.99.0 clippy --workspace --all-targets -- -D warnings`; `cargo build --workspace --all-targets`; DB tests via `scripts/e1-test-db.sh <batch> -- cargo test -p <crate> --test <name>` while iterating, and the full `scripts/e1-test-db.sh ci -- scripts/ci-run-tests.sh -- --test-threads=4` before the PR. Baseline measured 2026-10-08 on unmodified main: `synthesis_job_handler_test` 20/20 in 67 s, peak RSS 1.3 GB, ~0.75 GB disk.
- **Disk floor:** never create a second target dir; check `df -B1G / | awk 'NR==2{print $4}'` before each build; **stop (report blocked) below 9 GB free** — prod's Sunday pgBackRest full needs ~6 GB at backup time and ENOSPC crash-loops prod Postgres.

## Review Focus

1. **A theme whose members the viewer cannot read (or fewer than `MIN_READABLE_MEMBERS`)** → the tool refuses with a clear message and writes no pending row (Task 4 test `wiki_generate_refuses_thin_theme`).
2. **A group-private claim of another group sitting in the theme** → never seeded, never cited (Task 3 test `theme_seed_excludes_other_groups_claims`).
3. **Re-projection: a new theme UUID with the same `properties`** → same `wiki_key`, so the new article lands in the same page's history (Task 1 test `wiki_key_ignores_theme_uuid`, Task 5 test `history_spans_reprojected_theme`).
4. **A newer generation that failed** → the page keeps serving the previous complete article (Task 5 test `latest_complete_wins_over_newer_failed`).
5. **Embedder failure on a theme seed** → the job fails with a reason naming the embedding, not a misleading empty-result (Task 3 test `theme_seed_without_query_embedding_fails_with_reason`).

---

### Task 1: `episcience_core::wiki` — wiki key + seed selection (pure, no DB)

**Files:**
- Create: `crates/episcience-core/src/wiki/mod.rs`, `crates/episcience-core/src/wiki/key.rs`, `crates/episcience-core/src/wiki/seed.rs`
- Modify: `crates/episcience-core/src/lib.rs` (add `pub mod wiki;`)

**Interfaces:**
- Produces:
  - `pub const WIKI_SKILL_NAME: &str = "wiki_article"; pub const CANDIDATE_POOL: i32 = 400; pub const SEED_BUDGET: usize = 50; pub const DUP_COSINE: f32 = 0.95; pub const MMR_LAMBDA: f32 = 0.7; pub const MIN_READABLE_MEMBERS: i64 = 20;`
  - `pub struct WikiKey { pub run_id: Uuid, pub cluster_id: i64, pub split_part: Option<i64> }`
  - `impl WikiKey { pub fn from_properties(p: &serde_json::Value) -> Option<WikiKey>; pub fn as_slug(&self) -> String; pub fn parse_slug(s: &str) -> Option<WikiKey> }` — slug = `r{run_id as 32 lowercase hex}-c{cluster_id}` + `-s{split_part}` when present. The FULL run id is kept: a truncated prefix of a UUIDv7 run id is a timestamp and would merge histories of different runs.
  - `pub fn article_query(label: &str, description: &str) -> String` (`"{label}\n\n{description}"`, or just `label` when description is blank) and `pub fn title_from_query(q: &str) -> &str` (text before the first newline). A newline, not `". "`, separates them because labels contain periods ("U.S.", "e.g.").
  - `pub struct SeedCandidate { pub id: Uuid, pub relevance: f32, pub embedding: Vec<f32> }`
  - `pub fn select_article_seeds(cands: &[SeedCandidate], budget: usize, dup_cosine: f32, lambda: f32) -> Vec<Uuid>`

- [ ] **Step 1: Write the failing tests** (`key.rs` and `seed.rs` `#[cfg(test)] mod tests`)

```rust
// key.rs
#[test]
fn wiki_key_from_properties_with_and_without_split() {
    let p = serde_json::json!({"source":"cluster_run","cluster_id":197,"split_part":1,
        "cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b"});
    let k = WikiKey::from_properties(&p).unwrap();
    assert_eq!(k.as_slug(), "r16138781156b4e129b1d27f6ae8f9e8b-c197-s1");
    let p2 = serde_json::json!({"cluster_id":135,"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b"});
    assert_eq!(WikiKey::from_properties(&p2).unwrap().as_slug(), "r16138781156b4e129b1d27f6ae8f9e8b-c135");
}
#[test]
fn wiki_key_ignores_theme_uuid() {
    // Two projections of the same cluster (different theme ids, same properties) share a key.
    let p = serde_json::json!({"cluster_id":75,"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b","split_of":"38e3f1c7-116b-4e67-966c-23fe732d7d27"});
    assert_eq!(WikiKey::from_properties(&p).unwrap().as_slug(), "r16138781156b4e129b1d27f6ae8f9e8b-c75");
}
#[test]
fn uuidv7_runs_minted_close_together_get_distinct_keys() {
    // Same 48-bit timestamp prefix, different random tail.
    let a = serde_json::json!({"cluster_id":1,"cluster_run_id":"0192a1b2-c3d4-7000-8000-000000000001"});
    let b = serde_json::json!({"cluster_id":1,"cluster_run_id":"0192a1b2-c3d4-7000-8000-000000000002"});
    assert_ne!(WikiKey::from_properties(&a).unwrap().as_slug(), WikiKey::from_properties(&b).unwrap().as_slug());
}
#[test]
fn wiki_key_refuses_incomplete_properties() {
    assert!(WikiKey::from_properties(&serde_json::json!({"cluster_id":1})).is_none());
    assert!(WikiKey::from_properties(&serde_json::json!({"cluster_run_id":"not-a-uuid","cluster_id":1})).is_none());
    assert!(WikiKey::from_properties(&serde_json::json!({"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b","cluster_id":"7"})).is_none());
    assert!(WikiKey::from_properties(&serde_json::json!(null)).is_none());
}
#[test]
fn slug_parse_round_trips_and_rejects_garbage() {
    let p = serde_json::json!({"cluster_id":197,"split_part":1,"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b"});
    let k = WikiKey::from_properties(&p).unwrap();
    assert_eq!(WikiKey::parse_slug(&k.as_slug()), Some(k));
    let run = "16138781156b4e129b1d27f6ae8f9e8b";
    for bad in [String::new(), format!("r{}-c1", &run[..31]), format!("r{run}-c"), format!("r{run}-c1-s"),
                format!("x{run}-c1"), format!("r{run}-c1-s1-x"), format!("r{}g-c1", &run[..31]),
                format!("r{}-c1", run.to_uppercase()), format!("r{run}-c-1")] {
        assert!(WikiKey::parse_slug(&bad).is_none(), "{bad}");
    }
}
#[test]
fn article_query_and_title() {
    assert_eq!(article_query("Friction", "How friction works."), "Friction\n\nHow friction works.");
    assert_eq!(article_query("Friction", "  "), "Friction");
    assert_eq!(title_from_query("Friction\n\nHow friction works."), "Friction");
    assert_eq!(title_from_query("U.S. building codes. Dimensions"), "U.S. building codes. Dimensions");
    assert_eq!(title_from_query(&article_query("U.S. codes", "Scope.")), "U.S. codes");
}

// seed.rs
fn c(id: u128, rel: f32, e: &[f32]) -> SeedCandidate {
    SeedCandidate { id: Uuid::from_u128(id), relevance: rel, embedding: e.to_vec() }
}
#[test]
fn near_duplicates_collapse_to_the_more_relevant_one() {
    let cands = [c(1, 0.9, &[1.0, 0.0]), c(2, 0.8, &[0.999, 0.01]), c(3, 0.5, &[0.0, 1.0])];
    let got = select_article_seeds(&cands, 3, 0.95, 0.7);
    assert_eq!(got, vec![Uuid::from_u128(1), Uuid::from_u128(3)]);
}
#[test]
fn prefers_a_diverse_candidate_over_a_slightly_more_relevant_similar_one() {
    // 1 is picked first; 2 is close to 1 (cos≈0.89, below dup threshold) but 3 is orthogonal.
    let cands = [c(1, 0.90, &[1.0, 0.0]), c(2, 0.88, &[0.9, 0.45]), c(3, 0.80, &[0.0, 1.0])];
    let got = select_article_seeds(&cands, 2, 0.95, 0.7);
    assert_eq!(got, vec![Uuid::from_u128(1), Uuid::from_u128(3)]);
}
#[test]
fn respects_budget_and_handles_empty_and_degenerate_input() {
    assert!(select_article_seeds(&[], 5, 0.95, 0.7).is_empty());
    let cands: Vec<_> = (0..10).map(|i| c(i, 1.0 - i as f32 / 10.0, &[i as f32, 1.0])).collect();
    assert_eq!(select_article_seeds(&cands, 3, 0.95, 0.7).len(), 3);
    // A candidate with an empty or zero embedding is skipped, never a NaN pick.
    let odd = [c(1, 0.9, &[]), c(2, 0.8, &[0.0, 0.0]), c(3, 0.7, &[1.0, 0.0])];
    assert_eq!(select_article_seeds(&odd, 3, 0.95, 0.7), vec![Uuid::from_u128(3)]);
}
#[test]
fn ties_break_by_input_order_deterministically() {
    let cands = [c(5, 0.5, &[1.0, 0.0]), c(4, 0.5, &[0.0, 1.0])];
    assert_eq!(select_article_seeds(&cands, 1, 0.95, 0.7), vec![Uuid::from_u128(5)]);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `SQLX_OFFLINE=true cargo test -p episcience-core wiki::`
Expected: compile errors (module `wiki` not found).

- [ ] **Step 3: Implement**

```rust
// wiki/mod.rs
//! Wiki articles (plan: docs/superpowers/plans/2026-10-08-wiki-phase-b-articles.md).
//! An article is a `wiki_article` synthesis keyed by [`WikiKey`] (theme
//! `properties`, never the theme UUID — re-projection mints new ids).
pub mod key;
pub mod seed;
pub use key::{article_query, title_from_query, WikiKey};
pub use seed::{select_article_seeds, SeedCandidate};

pub const WIKI_SKILL_NAME: &str = "wiki_article";
/// Theme members pulled from the kernel before diversity selection.
pub const CANDIDATE_POOL: i32 = 400;
/// Seeds per article (the spike's recall limit; the cost budget of 20 LLM calls holds at ~13).
pub const SEED_BUDGET: usize = 50;
/// Cosine at or above which two candidates are the same fact restated.
pub const DUP_COSINE: f32 = 0.95;
/// MMR weight on relevance (1 - weight on novelty).
pub const MMR_LAMBDA: f32 = 0.7;
/// An article exists only when the generating group can read at least this many members.
pub const MIN_READABLE_MEMBERS: i64 = 20;
```

```rust
// wiki/key.rs
use uuid::Uuid;

/// Stable identity of a wiki page: the theme's clustering provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiKey {
    pub run_id: Uuid,
    pub cluster_id: i64,
    pub split_part: Option<i64>,
}

impl WikiKey {
    /// From `claim_themes.properties`. `None` unless `cluster_run_id` is a uuid
    /// string and `cluster_id` an integer; `split_part` is optional.
    pub fn from_properties(p: &serde_json::Value) -> Option<WikiKey> {
        let run_id = Uuid::parse_str(p.get("cluster_run_id")?.as_str()?).ok()?;
        let cluster_id = p.get("cluster_id")?.as_i64()?;
        let split_part = p.get("split_part").and_then(|v| v.as_i64());
        Some(WikiKey { run_id, cluster_id, split_part })
    }

    pub fn as_slug(&self) -> String {
        let run = self.run_id.simple();
        match self.split_part {
            Some(s) => format!("r{run}-c{}-s{s}", self.cluster_id),
            None => format!("r{run}-c{}", self.cluster_id),
        }
    }

    /// Inverse of [`Self::as_slug`]; `None` for anything else (URL input).
    pub fn parse_slug(s: &str) -> Option<WikiKey> {
        let rest = s.strip_prefix('r')?;
        let mut parts = rest.split('-');
        let run = parts.next()?;
        if run.len() != 32 || !run.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        let digits = |t: &str| -> Option<i64> {
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) { return None; }
            t.parse().ok()
        };
        let cluster_id = digits(parts.next()?.strip_prefix('c')?)?;
        let split_part = match parts.next() {
            None => None,
            Some(p) => Some(digits(p.strip_prefix('s')?)?),
        };
        if parts.next().is_some() {
            return None;
        }
        Some(WikiKey { run_id: Uuid::parse_str(run).ok()?, cluster_id, split_part })
    }
}

/// The synthesis query a wiki article is generated from (also its title source).
pub fn article_query(label: &str, description: &str) -> String {
    if description.trim().is_empty() { label.to_string() } else { format!("{label}\n\n{description}") }
}

/// The article title: the label line of [`article_query`].
pub fn title_from_query(q: &str) -> &str {
    q.split_once('\n').map(|(t, _)| t).unwrap_or(q)
}
```

```rust
// wiki/seed.rs
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SeedCandidate {
    pub id: Uuid,
    /// Similarity to the article query (higher = more on-topic).
    pub relevance: f32,
    pub embedding: Vec<f32>,
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.is_empty() || a.len() != b.len() { return None; }
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) { d += x * y; na += x * x; nb += y * y; }
    if na == 0.0 || nb == 0.0 { return None; }
    Some(d / (na.sqrt() * nb.sqrt()))
}

/// Maximal-marginal-relevance selection with near-duplicate suppression.
/// Greedy: each round picks the candidate maximising
/// `lambda * relevance - (1 - lambda) * max_cos_to_selected`; a candidate whose
/// cosine to any selected one is `>= dup_cosine` is dropped (same fact restated).
/// Candidates with an unusable embedding (empty, zero, wrong length) are skipped.
/// Ties keep input order.
pub fn select_article_seeds(cands: &[SeedCandidate], budget: usize, dup_cosine: f32, lambda: f32) -> Vec<Uuid> {
    let dim = cands.iter().find(|c| cosine(&c.embedding, &c.embedding).is_some()).map(|c| c.embedding.len());
    let mut pool: Vec<&SeedCandidate> = cands
        .iter()
        .filter(|c| Some(c.embedding.len()) == dim && cosine(&c.embedding, &c.embedding).is_some())
        .collect();
    let mut picked: Vec<&SeedCandidate> = Vec::new();
    while picked.len() < budget && !pool.is_empty() {
        let mut best: Option<(usize, f32)> = None;
        for (i, c) in pool.iter().enumerate() {
            let redundancy = picked.iter().filter_map(|p| cosine(&c.embedding, &p.embedding)).fold(0f32, f32::max);
            let score = lambda * c.relevance - (1.0 - lambda) * redundancy;
            if best.map_or(true, |(_, s)| score > s) { best = Some((i, score)); }
        }
        let (i, _) = best.expect("pool non-empty");
        let chosen = pool.remove(i);
        pool.retain(|c| cosine(&c.embedding, &chosen.embedding).map_or(true, |s| s < dup_cosine));
        picked.push(chosen);
    }
    picked.into_iter().map(|c| c.id).collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `SQLX_OFFLINE=true cargo test -p episcience-core wiki::`
Expected: 10 passed. Then `cargo +1.99.0 clippy -p episcience-core --all-targets -- -D warnings` clean (fix any `map_or(true, …)` lint by using `is_none_or` if clippy asks).

- [ ] **Step 5: Commit** — `feat(wiki): add wiki page key and diverse seed selection`

---

### Task 2: `WikiArticleSkill`

**Files:**
- Create: `crates/episcience-core/src/synthesis/skills/wiki_article.rs`, `crates/episcience-core/src/synthesis/skills/markdown/wiki_article.md`
- Modify: `crates/episcience-core/src/synthesis/skills/mod.rs` (add `pub mod wiki_article;` and the `"wiki_article"` arm in `load_by_name`)

**Interfaces:**
- Consumes: `SynthesisSkill`, `SynthesisStage` (`crates/episcience-core/src/synthesis/skill.rs`); `crate::wiki::WIKI_SKILL_NAME`.
- Produces: `pub struct WikiArticleSkill;` registered as `load_by_name("wiki_article")`.

- [ ] **Step 1: Failing tests** (in `wiki_article.rs`, plus one in `skills/mod.rs` tests)

```rust
#[test]
fn wiki_article_skill_shapes_an_encyclopedic_article() {
    let s = WikiArticleSkill;
    assert_eq!(s.name(), crate::wiki::WIKI_SKILL_NAME);
    let comp = s.section(SynthesisStage::Composition).unwrap();
    for must in ["lede", "## Contested", "## Gaps", "<<<CLUSTER:", "verbatim"] {
        assert!(comp.contains(must), "composition prompt lacks {must:?}");
    }
    // stage5_compose rejects any edit inside a cluster block (ComposeAnchorViolation), so the
    // composition guidance must never ask to merge, reorder-within, or rewrite cluster text.
    let lc = comp.to_lowercase();
    for banned in ["merge", "rewrite", "paraphrase", "do not restate"] {
        assert!(!lc.contains(banned), "composition prompt asks for {banned:?}, which breaks the anchor validator");
    }
    let narr = s.section(SynthesisStage::Narration).unwrap();
    assert!(narr.contains("[<claim_id>]"));
    assert!(narr.to_lowercase().contains("disagree"));
    // No traversal opinion: production has no edge provider (plan, "Spike evidence").
    assert!(s.traversal_config().is_none());
    assert!(s.section(SynthesisStage::Verification).is_none());
}
// skills/mod.rs tests
#[test]
fn load_by_name_returns_wiki_article() {
    assert_eq!(load_by_name("wiki_article").unwrap().name(), "wiki_article");
}
```

- [ ] **Step 2: Run** `SQLX_OFFLINE=true cargo test -p episcience-core skills::` — expect compile failure.

- [ ] **Step 3: Implement** (mirror `literature.rs`'s shape)

```rust
//! `WikiArticleSkill` — an encyclopedic article about one theme, for the wiki
//! (plan 2026-10-08-wiki-phase-b-articles.md). Seeds are theme-anchored and
//! de-duplicated upstream (Task 3); this skill shapes the prose.
//! Verification inherits the default citation rubric (every member cited).
use crate::synthesis::skill::{SynthesisSkill, SynthesisStage};

#[derive(Debug, Default)]
pub struct WikiArticleSkill;

#[async_trait::async_trait]
impl SynthesisSkill for WikiArticleSkill {
    fn name(&self) -> &'static str { crate::wiki::WIKI_SKILL_NAME }

    fn section(&self, stage: SynthesisStage) -> Option<&str> {
        Some(match stage {
            SynthesisStage::Overview => {
                "Write an encyclopedia article about one topic for readers who know \
                 nothing about it. Neutral, factual tone; no first person."
            }
            SynthesisStage::Narration => {
                "Summarise this cluster as one or two encyclopedic paragraphs. Cite \
                 every claim as `[<claim_id>]`. If claims disagree or qualify each \
                 other, say so explicitly and cite both sides. Within this cluster, \
                 state a fact once even if several claims repeat it; cite all of them \
                 on that sentence."
            }
            SynthesisStage::Composition => {
                "Write an encyclopedia article around the cluster blocks: a `# Title` \
                 line, then a lede paragraph that defines the topic in its first \
                 sentence, then the cluster blocks in a logical teaching order, each \
                 preceded by a `##` heading you write. Your own text goes only between \
                 blocks; every `<<<CLUSTER:{id}:BEGIN/END>>>` block stays verbatim, \
                 unchanged inside. If any cluster reported disagreement, add \
                 `## Contested` after the last block, listing each disagreement with \
                 citations to both sides. End with `## Gaps` naming what the cited \
                 claims do not cover."
            }
            _ => return None,
        })
    }
}
```

Write `markdown/wiki_article.md` as the human-readable reference (same structure as `markdown/literature.md`: purpose, stage overrides verbatim, rationale citing the spike's repetition and off-theme findings).

- [ ] **Step 4: Run** `SQLX_OFFLINE=true cargo test -p episcience-core skills::` — all pass; clippy 1.99 clean.

- [ ] **Step 5: Commit** — `feat(synthesis): add the wiki_article synthesis skill`

---

### Task 3: Theme-anchored Stage 1 seed

**Files:**
- Modify: `crates/episcience-db/src/synthesis/pipeline.rs` (new `stage1_seed_theme` in the unbounded `impl<L, P>` block next to `stage1_seed`)
- Modify: `crates/episcience-api/src/jobs/synthesis_job.rs` (`SynthesisJobPayload.seed_theme_id`; branch in `run_stages` step 4)
- Test: `crates/episcience-api/tests/synthesis_job_handler_test.rs` (new tests + fixtures)

**Interfaces:**
- Consumes: `episcience_core::wiki::{select_article_seeds, SeedCandidate, CANDIDATE_POOL, SEED_BUDGET, DUP_COSINE, MMR_LAMBDA}`; `epigraph_db::repos::claim_theme::ClaimThemeRepository::claims_in_themes_at_dim(pool, viewer, &[Uuid], query_vec: &str, limit: i32, centroid_dim: u32, paragraph_only: bool) -> Result<Vec<(Uuid, String, f64)>, DbError>`; `epigraph_embeddings::normalizer::Normalizer::format_as_pgvector(&[f32]) -> String`; `EmbeddingService::get(claim_id) -> Result<Vec<f32>, EmbeddingError>`.
- Produces:
  - `SynthesisJobPayload { …, #[serde(default)] pub seed_theme_id: Option<Uuid> }`
  - `pub async fn stage1_seed_theme(&self, viewer: &Viewer, theme_id: Uuid) -> Result<Vec<Uuid>, SynthesisError>`

- [ ] **Step 1: Failing tests** (follow the file's existing fixture style: fixtures on the clone's superuser pool, the job run via `SynthesisJobHandler::run` on an `OwnerSession`). Add an embedder double whose `generate_query` returns `e(0)` and whose `get(id)` returns the fixture embedding for `id` (keep a `HashMap<Uuid, Vec<f32>>`). Fixture helper `seed_theme_fixture(pool) -> ThemeFixture` inserts one `claim_themes` row (label "Origami folding", properties `{"cluster_run_id": <uuid>, "cluster_id": 7}`) and claims with `theme_id = T` and 1536-dim `embedding` values built by `e(k)` = unit vector on axis k:
  - 3 public on-theme claims A, B, C (axes 1, 2, 3); D = a near-duplicate of A (axis 1 with 0.001 on axis 4) also public on-theme;
  - E = public, **not** in theme T, but with embedding `e(0)` (the query vector — a text seed would pick it first);
  - F = in theme T, `visibility = 'group'`, `owner_group_id` = a group the acting principal is **not** in.

```rust
#[tokio::test]
async fn theme_seed_anchors_to_theme_and_drops_duplicates() {
    // payload.seed_theme_id = Some(T); run the job; read synthesis_claim_membership.
    // Expect members ⊆ {A,B,C,D}, contains A,B,C, does NOT contain both A and D, never E.
}
#[tokio::test]
async fn theme_seed_excludes_other_groups_claims() {
    // Same run: F is never a member, never cited in the narrative.
    // NOTE (doc comment on the test): on main the engine pool reads PUBLIC claims only
    // (`V1-engine-takes-pool`, until KE-1), so this passes for that reason today; it pins the
    // guarantee for when the engine reads as the stamped viewer. It is not coverage of stage-2 scoping.
}
#[tokio::test]
async fn text_seed_path_is_unchanged_when_no_theme() {
    // payload.seed_theme_id = None → existing origami recall behaviour (2 seeds), as today.
}
#[tokio::test]
async fn theme_seed_without_query_embedding_fails_with_reason() {
    // Embedder double whose generate_query errors; seed_theme_id = Some(T).
    // Expect RunError::Failed and syntheses.failure_reason containing "query embedding".
}
#[tokio::test]
async fn old_payload_without_seed_theme_id_still_deserialises() {
    let v = serde_json::json!({"synthesis_id": Uuid::nil(), "query": "q", "traversal_config": null,
        "agent_id": Uuid::nil(), "parent_synthesis_id": null});
    let p: SynthesisJobPayload = serde_json::from_value(v).unwrap();
    assert!(p.seed_theme_id.is_none());
}
```

- [ ] **Step 2: Run** `scripts/e1-test-db.sh wikib -- cargo test -p episcience-api --test synthesis_job_handler_test theme_` — expect compile failure (`seed_theme_id` unknown).

- [ ] **Step 3: Implement**

In `pipeline.rs`:

```rust
    /// Stage 1 — Seed, theme-anchored (wiki articles). Candidates are the
    /// theme's members `viewer` can read (the kernel splices the visibility
    /// predicate), ranked by similarity to `self.query_embedding`; then
    /// [`select_article_seeds`] picks a diverse, de-duplicated subset. The
    /// caller still runs the seed filter (public + owner group only).
    ///
    /// # Errors
    /// [`SynthesisError::Validation`] without a query embedding;
    /// [`SynthesisError::Db`] on a kernel read failure;
    /// [`SynthesisError::EmptyResult`] when nothing survives.
    pub async fn stage1_seed_theme(&self, viewer: &Viewer, theme_id: Uuid) -> Result<Vec<Uuid>, SynthesisError> {
        use episcience_core::wiki::{select_article_seeds, SeedCandidate, CANDIDATE_POOL, DUP_COSINE, MMR_LAMBDA, SEED_BUDGET};
        if self.query_embedding.is_empty() {
            return Err(SynthesisError::Validation(
                "theme seed needs a query embedding (embedder unavailable)".into(),
            ));
        }
        let qv = epigraph_embeddings::normalizer::Normalizer::format_as_pgvector(&self.query_embedding);
        let rows = epigraph_db::repos::claim_theme::ClaimThemeRepository::claims_in_themes_at_dim(
            &self.pool, viewer, &[theme_id], &qv, CANDIDATE_POOL, 1536, false,
        )
        .await
        .map_err(|e| SynthesisError::Db(e.to_string()))?;
        let mut cands = Vec::with_capacity(rows.len());
        for (id, _content, sim) in rows {
            // A member without a stored embedding cannot be diversity-ranked; skip it.
            if let Ok(embedding) = self.embedder.get(id).await {
                cands.push(SeedCandidate { id, relevance: sim as f32, embedding });
            }
        }
        let seeds = select_article_seeds(&cands, SEED_BUDGET, DUP_COSINE, MMR_LAMBDA);
        if seeds.is_empty() { return Err(SynthesisError::EmptyResult); }
        Ok(seeds)
    }
```

(If `claims_in_themes_at_dim` is not `pub` at the pinned rev or takes `&PgPool` vs `PgExecutor`, call `epigraph_engine::diverse_retrieval::candidates_in_themes_at_dim(pool, viewer, &[theme_id], &qv, CANDIDATE_POOL, 1536, false)` instead — same return shape, `sqlx::Error`.)

In `synthesis_job.rs`: add the payload field with a doc comment ("theme-anchored seed for wiki articles; `None` = text recall"); in `run_stages` step 4 replace the `stage1_seed` call with

```rust
        let seeds = match payload.seed_theme_id {
            Some(theme_id) => pipeline.stage1_seed_theme(&owner, theme_id).await?,
            None => pipeline.stage1_seed(&owner, &payload.query, 50, 0.5).await?,
        };
```

and keep the existing `seed_filter` block unchanged after it. Update the step-2 soft-fail comment: a theme seed fails closed without the embedding.

- [ ] **Step 4: Run** the five tests + the whole `synthesis_job_handler_test` (no regressions).

- [ ] **Step 5: Commit** — `feat(synthesis): seed wiki articles from their theme's readable members`

---

### Task 4: Schema columns + `wiki_generate_article` MCP tool

**Files:**
- Create: `migrations/5041_wiki_article_columns.sql`
- Modify: `crates/episcience-db/src/repos/synthesis.rs` (add `set_wiki_seed_tx`)
- Create: `crates/episcience-api/src/mcp/wiki.rs`
- Modify: `crates/episcience-api/src/mcp/mod.rs` (register the tool; scope gate like `synthesize`), `crates/episcience-api/src/mcp/synthesize.rs` (extract the shared enqueue + poll so both tools use one path)
- Modify: `migrations/README.md` (layout entry for 5041)
- Test: `crates/episcience-api/tests/mcp_write_tools_test.rs`, plus whatever ratchet tests the lint/tenancy suites require for a new column (run `cargo test -p episcience-db --test migration_lint` and the tenancy ratchets and follow their failure messages)

**Interfaces:**
- Consumes: Task 1 (`WikiKey`, `article_query`, `MIN_READABLE_MEMBERS`, `WIKI_SKILL_NAME`), Task 3 payload field.
- Produces:
  - columns `public.syntheses.seed_theme_id uuid NULL`, `public.syntheses.wiki_key text NULL`
  - `SynthesisRepository::set_wiki_seed_tx(executor, id: Uuid, seed_theme_id: Uuid, wiki_key: &str) -> Result<(), DbError>`
  - MCP tool `wiki_generate_article(theme_id: Uuid, owner_group_id: Option<Uuid>, wait_for_completion: bool, timeout_seconds: u64) -> { synthesis_id, wiki_key, status, narrative? }`

- [ ] **Step 1: Migration**

```sql
-- 5041: wiki articles are syntheses (plan 2026-10-08-wiki-phase-b-articles.md).
-- seed_theme_id: the claim_themes row the seed was drawn from (provenance only,
--   no FK: themes are re-projected with new ids).
-- wiki_key: the page key from the theme's clustering provenance (WikiKey::as_slug).
SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

ALTER TABLE public.syntheses
    ADD COLUMN seed_theme_id uuid,
    ADD COLUMN wiki_key text;

ALTER TABLE public.syntheses
    ADD CONSTRAINT syntheses_wiki_key_shape
        CHECK (wiki_key IS NULL OR wiki_key ~ '^r[0-9a-f]{32}-c[0-9]+(-s[0-9]+)?$'),
    ADD CONSTRAINT syntheses_wiki_pair
        CHECK ((wiki_key IS NULL) = (seed_theme_id IS NULL));

CREATE INDEX syntheses_wiki_page_idx
    ON public.syntheses (owner_group_id, wiki_key, created_at DESC)
    WHERE wiki_key IS NOT NULL;
```

Run `cargo test -p episcience-db --test migration_lint` (no DB) and, on a test DB, `episcience-migrate run` + `verify` (CI step "episcience-migrate verify against a fresh template"). If the baseline fingerprint / tenancy ratchets name `syntheses` columns, update them as their failure messages direct.

- [ ] **Step 2: Failing tests** (`mcp_write_tools_test.rs`, following its existing caller/fixture helpers; the theme fixture from Task 3 can be shared via a `support` helper)

```rust
#[tokio::test] async fn wiki_generate_creates_a_group_wiki_synthesis() {
    // Caller is a member of group G; theme T has ≥ MIN_READABLE_MEMBERS readable members.
    // Expect a syntheses row: skill_name='wiki_article', visibility='group', owner_group_id=G,
    // seed_theme_id=T, wiki_key='r<run uuid as 32 hex>-c7', query = article_query(label, description),
    // and a synthesis_jobs payload with "seed_theme_id": T.
}
#[tokio::test] async fn wiki_generate_refuses_thin_theme() {
    // Theme with fewer readable members than MIN_READABLE_MEMBERS → invalid_request mentioning the count;
    // no syntheses row and no job written.
}
#[tokio::test] async fn wiki_generate_refuses_theme_without_cluster_properties() { /* properties = {} → invalid_request; nothing written */ }
#[tokio::test] async fn wiki_generate_refuses_unknown_theme() { /* random uuid → invalid_request "theme … not found" */ }
#[tokio::test] async fn wiki_generate_refuses_a_group_the_caller_cannot_write() { /* owner_group_id = H (not a member) → refused by root_ownership; nothing written */ }
```

- [ ] **Step 3: Implement**
  - `set_wiki_seed_tx`: `UPDATE public.syntheses SET seed_theme_id = $2, wiki_key = $3 WHERE id = $1` (runtime `sqlx::query`), called in the same transaction right after `create_pending_tx`.
  - `mcp/wiki.rs` `handle(server, auth, viewer, args)`:
    1. `let mut tx = server.db.write_as(viewer).await?;`
    2. Theme: `ClaimThemeRepository::get_summary(pool?, viewer, theme_id)` — it takes `&PgPool`; if the request path forbids pool reads (`no_unscoped_pool`), replicate its SELECT on `&mut *tx` instead (the viewer-stamped connection), selecting `t.label, t.description, t.properties` plus the live readable member count `COUNT(*) FROM claims c WHERE c.theme_id = t.id AND COALESCE(c.is_current, true)` (RLS on the stamped connection does the visibility filtering). `None` → `invalid_request("theme {id} not found")`.
    3. `member_count < MIN_READABLE_MEMBERS` → `invalid_request(format!("theme has {n} readable members; a wiki article needs at least {MIN_READABLE_MEMBERS}"))`.
    4. `WikiKey::from_properties(&properties)` or `invalid_request("theme has no cluster provenance (cluster_run_id, cluster_id)")`.
    5. Ownership: `root_ownership(&mut tx, viewer, args.owner_group_id, Visibility::Group-resolved)` exactly as `synthesize` does for a root with requested visibility `"group"`.
    6. Shared enqueue (extracted from `synthesize::handle` into e.g. `synthesize::enqueue(tx, server, auth, EnqueueSpec { query, owner, skill_name, parent: None, prereqs: &[], traversal_config: None, seed_theme_id: Option<Uuid> })` returning the id), then `set_wiki_seed_tx`, commit, then the shared poll loop when `wait_for_completion`.
  - Register in `mcp/mod.rs` with a `#[tool(description = "Generate (or regenerate) the wiki article for a theme as a group synthesis owned by owner_group_id (default: the caller's group). Refuses themes with fewer than 20 readable members or without cluster provenance.")]`, the same scope gate as `synthesize`, and add `wiki_generate_article` to the server instructions string's tool list. Startup tool count is derived (`fix(mcp): derive the startup tool count`), so no hardcoded count to bump — check `mcp_tools_test.rs` for a tool-name list and add it there.

- [ ] **Step 4: Run** the new tests, the whole `mcp_write_tools_test` and `mcp_tools_test`, `no_unscoped_pool`, `migration_lint`, tenancy ratchets.

- [ ] **Step 5: Commit** (two commits): `feat(db): add wiki_key and seed_theme_id to syntheses` (migration + repo + ratchet updates), then `feat(mcp): add wiki_generate_article tool`.

---

### Task 5: Wiki registry reads + REST routes for Phase C

**Files:**
- Create: `crates/episcience-db/src/repos/wiki.rs` (+ export in `crates/episcience-db/src/repos/mod.rs` / `lib.rs` like the other repos)
- Create: `crates/episcience-api/src/routes/wiki.rs`; Modify `crates/episcience-api/src/routes/mod.rs` (mount under `/api/v1/eln/wiki`, same auth layer as `syntheses.rs`)
- Test: `crates/episcience-api/tests/wiki_routes_test.rs` (pattern: `syntheses_routes_test.rs`)

**Interfaces:**
- Consumes: Task 1 `title_from_query`, `WikiKey::parse_slug`; Task 4 columns.
- Produces:
  - `pub struct WikiPageRow { pub owner_group_id: Uuid, pub wiki_key: String, pub title: String, pub synthesis_id: Uuid, pub generated_at: DateTime<Utc>, pub stale_since: Option<DateTime<Utc>> }`
  - `WikiRepository::list_pages(conn: &mut PgConnection) -> Result<Vec<WikiPageRow>, DbError>` — latest **complete** row per `(owner_group_id, wiki_key)`:
    ```sql
    SELECT DISTINCT ON (s.owner_group_id, s.wiki_key)
           s.owner_group_id, s.wiki_key, s.query, s.id, s.completed_at, s.stale_since
      FROM public.syntheses s
     WHERE s.wiki_key IS NOT NULL AND s.status = 'complete' AND s.skill_name = 'wiki_article'
     ORDER BY s.owner_group_id, s.wiki_key, s.completed_at DESC, s.id DESC
    ```
    (RLS on the viewer-stamped connection limits rows to readable syntheses; `title = title_from_query(query)`.)
  - `WikiRepository::get_page(conn, group_id: Uuid, wiki_key: &str) -> Result<Option<WikiPageDetail>, DbError>` where `WikiPageDetail { page: WikiPageRow, narrative: String, history: Vec<WikiVersion { synthesis_id, status, created_at, completed_at }> }` (history = every row for the key, newest first, any status).
  - `GET /api/v1/eln/wiki` → `[WikiPageRow]` JSON; `GET /api/v1/eln/wiki/:group_id/:wiki_key` → `WikiPageDetail` JSON, **404** when the slug is malformed (`WikiKey::parse_slug` is `None`), the group is not readable, or no complete version exists — never an empty 200.

- [ ] **Step 1: Failing tests**

```rust
#[tokio::test] async fn list_shows_only_pages_of_readable_groups() { /* pages for G and H; viewer in G sees only G's */ }
#[tokio::test] async fn latest_complete_wins_over_newer_failed() { /* v1 complete, v2 failed (newer) → page serves v1; history lists both, v2 first */ }
#[tokio::test] async fn history_spans_reprojected_theme() { /* two complete versions, different seed_theme_id, same wiki_key → one page, history of 2 */ }
#[tokio::test] async fn get_page_404s_for_other_group_bad_slug_and_no_complete_version() { /* three cases, each 404 */ }
#[tokio::test] async fn stale_page_reports_stale_since() { /* set stale_since on the current version → field present in list and detail */ }
```

Fixtures insert `syntheses` rows directly on the superuser pool (status, completed_at, wiki_key, seed_theme_id, owner_group_id, visibility='group', narrative) — follow `syntheses_routes_test.rs`.

- [ ] **Step 2: Run** `scripts/e1-test-db.sh wikib -- cargo test -p episcience-api --test wiki_routes_test` — compile failure.

- [ ] **Step 3: Implement** the repo (runtime `sqlx::query_as` with an explicit column list) and the two handlers on `server.db.read_as(viewer)` (copy the `get_synthesis` handler's auth/viewer extraction).

- [ ] **Step 4: Run** the new tests + `no_unscoped_pool` + `rest_auth_gate_test` / `rest_scope_gate_test` (add the two routes to whatever route inventory those tests keep).

- [ ] **Step 5: Commit** — `feat(api): add wiki page registry reads and routes`

---

### Task 6: Docs — plan update, curator runbook

**Files:**
- Modify: `docs/superpowers/plans/2026-10-07-epigraph-wiki.md` (Phase B section: link this plan; record the decisions — per-group curator, registry = syntheses columns, "See also" moved to C, Contested is LLM-reported until the real edge provider; Phase 0 result GO with the spike evidence bullets)
- Create: `docs/runbooks/wiki-curator-agent.md` — operator steps to provision the group:main curator: create the agent identity by the kernel's normal agent-registration path, grant membership in **group:main only**, mint its client credentials into the EpiClaw secret store (Phase D reads them), and verify with a read-only check that it belongs to exactly one group. States plainly that no code path creates this identity and that a curator in two groups breaks the visibility guarantee (2-hop traversal reads with the acting principal's full scope once a real edge provider lands).
- Modify: `docs/deploy.md` only if the tool list / routes are documented there.

- [ ] **Step 1:** Write both docs. **Step 2:** `cargo fmt --all -- --check` and a full local gate (see Global Constraints). **Step 3: Commit** — `docs(wiki): record Phase B decisions and the curator runbook`.

---

## Out of scope (named so nobody builds them here)

- Phase C Explorer `/wiki` pages and "See also" (Explorer side, uses Task 5 routes).
- Phase D nightly `wiki-curator` schedule and Telegram summary (EpiClaw).
- A real `EdgeProvider` and edge-backed Contested sections (B-CKL Phase 4).
- Deploying anything: prod episcience is the Aug-2 build with a bypass DSN; redeploy is operator-gated (ops-private DECISIONS addendum 2026-10-07).
