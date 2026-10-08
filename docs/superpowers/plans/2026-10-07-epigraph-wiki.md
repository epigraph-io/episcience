# EpiGraph Wiki Plan

> **For agentic workers:** this is a phase-level plan. Expand a phase into
> task-level steps (superpowers:writing-plans) before implementing it.

**Goal:** an automatically generated wiki over the EpiGraph corpus. It has one
cited article per top-level theme, separately for each group, served by the
Explorer and refreshed nightly by EpiClaw, with a Telegram summary of what
changed.

**Shape of the idea:** Karpathy's "LLM Wiki" pattern (raw sources → LLM-written,
interlinked pages → a schema; ingest / query / lint), with one deliberate
difference. **The graph stays the source of truth.** Every article is an
episcience synthesis: derived, cited claim by claim, carrying the belief
intervals it was written against, marked stale when those claims move, and
regenerable at any time. Nothing in the wiki is edited by hand or by the LLM in
place.

**Decisions taken (2026-10-07):**

| # | Question | Decision |
|---|---|---|
| 1 | Visibility | **One wiki per group.** An article is written with that group's read scope and shared only with that group. |
| 2 | Where the article registry lives | **episcience**, next to the syntheses. |
| 3 | Nightly budget | **5 articles per night**, with an EpiClaw Telegram summary to the operator. |
| 4 | What an article is about | **Top-level themes** from the theming subsystem, not raw labels. |
| 5 | First group | **`group:main`.** |
| 6 | `theme_cluster` safety against probes | **Out of scope here**, tracked as a high-priority backlog item (5abe23de). |

---

## What already exists

- **Synthesis pipeline (episcience).**
  - Takes a query and traverses up to 2 hops over Supports / Contradicts /
    Supersedes / Methodology / Corroborates edges, with a capped subgraph size.
  - Clusters the subgraph with signed Louvain and writes a markdown narrative
    citing claim ids, under a pluggable `SynthesisSkill` (`baseline`,
    `lab_notebook`, `code_review`, …).
  - Stores a subgraph snapshot with per-claim belief intervals, and
    parent/prerequisite synthesis ids for linking.
  - A staleness worker marks a synthesis stale when member claims receive
    belief updates. `synthesis_shares` handles sharing.
- **Theme-v2 pipeline (epigraph `scripts/`).** `cluster_claims.py`,
  `subcluster_outliers.py`, `refine_clusters.py`, `label_themes_llm.py`,
  `maintain_themes.py` and `project_to_themes.py`. It clusters in UMAP space
  (cosine, L2-normalised), names clusters with an LLM, and projects them onto
  `claim_themes` / `claims.theme_id`, the tables recall, the Explorer and MCP
  read. It was validated on a production clone, with essentially the whole
  corpus themed, but **never promoted to production** (backlog c1b7dd8a).
- **Rust `theme_cluster`.** It is being retired (backlog a0ed0180, 0c3650af):
  its sample is fixed and small, its k selection is inert, and it deletes every
  theme by default. **Do not build on it.**
- **Explorer.** It is read-only, server-rendered, reads through the viewer's own
  token, and already has theme and claim pages to link into.

## Phases

| Phase | Repo | What ships | Rough size |
|---|---|---|---|
| A | epigraph (scripts) | About 200 named top-level themes for the whole corpus, in `claim_themes` | 3–5 days |
| 0 | episcience | Synthesis spike, with a go/no-go decision | 1 day, parallel to A |
| B | episcience | `WikiArticleSkill`, a per-group article registry, group-scoped generation | ~3 days |
| C | epigraph (`services/explorer`) | `/wiki` routes | 2–3 days |
| D | epiclaw-host | Nightly `wiki-curator` schedule with Telegram summary | 1–2 days |

### A — Top-level themes (prerequisite, critical path)

1. Run the theme-v2 pipeline over the full current corpus to produce
   fine-grained themes.
2. Cluster the **theme centroids** in UMAP space into **about 200 top-level
   themes**. Each top-level theme gets an LLM-written name and a one-paragraph
   scope note, which become the article title and lede.
3. Project the top-level themes onto `claim_themes` / `claims.theme_id` (via
   `project_to_themes.py`) so every existing reader sees them.
4. Keep theme ids **stable across re-runs**: match new centroids to old ones
   rather than wiping. Article URLs and staleness tracking depend on it.
5. Run the heavy steps (UMAP, clustering) off the API host. Only the projection
   writes touch the production database.

**Hierarchical decomposition comes later** (top-level → sub-themes). Keep the
fine-grained layer from step 1, because it becomes the second level without
re-clustering.

### 0 — Synthesis spike

Hand-run `synthesize` on five subjects. Measure:

- wall time and LLM calls per article;
- the failure rate (a recent run failed with "compose stage violated cluster
  anchor");
- whether every citation resolves to a claim the reader can open.

**Go/no-go:** at least 4 of 5 complete, and their citations resolve.

**Result (2026-10-07, prod, the Aug-2 build): GO.** 5 of 5 syntheses
completed, 226 of 226 citations resolved, about 3–4 minutes and about 13 LLM
calls per article. The full write-up is private (in ops-private). What it
changed in B:

- **Text seeding drifts off-theme.** 206 of 210 themes have no scope note, so
  the label is the only text handle. A label-only seed for "Building Code
  Dimensional Requirements" cited none of its own theme's claims (37 of 42 came
  from a larger neighbouring theme); another cited 42 of 50. ⇒ B seeds from the
  theme's members, not from text.
- **Repetition.** Near-duplicate claims (the same textbook fact across
  editions) were restated cluster after cluster. Composition cannot remove
  them: `stage5_compose` requires every cluster summary verbatim inside its
  sentinels and rejects any edit (`ComposeAnchorViolation`, the one failure
  seen before the spike). ⇒ B suppresses near-duplicates at seed time and tells
  the skill to narrate them once within a cluster.
- **No traversal in production.** The worker runs with `EmptyEdgeProvider`, so
  an article is its ≤ 50 seeds and has no contradiction edges. ⇒ Contested is
  LLM-reported, and "See also" moves to C.
- **Coverage.** 50 seeds against 1k–7k members. ⇒ seeds are chosen for
  diversity (MMR), not as the nearest 50.

### B — Articles in episcience

**Task plan:** [`2026-10-08-wiki-phase-b-articles.md`](2026-10-08-wiki-phase-b-articles.md).
The bullets below are what B ships; where they differ from this plan's first
draft, the first draft is what changed (see "Decisions taken for B").

- **`WikiArticleSkill`** (`skill_name = 'wiki_article'`), a `SynthesisSkill`
  that writes an encyclopedic article: a lede, sections by sub-cluster, a
  "Contested" section and a "Gaps" section. It states a fact once even when
  several claims repeat it, and cites all of them.
- **A theme-anchored seed.** A synthesis today starts from a text query. A wiki
  synthesis instead seeds Stage 1 from theme T's current members that the
  generating agent can read (the kernel's `claims_in_themes_at_dim`, a candidate
  pool of 400). From that pool it picks at most 50 seeds by MMR (λ = 0.7) and
  suppresses near-duplicates (cosine ≥ 0.95), so the seeds cover the theme
  rather than its densest corner. The existing seed filter still keeps only
  public claims and the article's owner group's own claims. If the embedder
  fails, the job fails with a reason naming the embedding, not with an empty
  article.
- **The registry is two columns on `syntheses`, not a `wiki_pages` table.**
  Migration 5042 adds `seed_theme_id` and `wiki_key`. The key is the theme's
  clustering provenance (`cluster_run_id`, `cluster_id`, `split_part` from
  `claim_themes.properties`), never the theme UUID, so a re-projected theme
  keeps its page and history. A page is the latest **complete**
  `wiki_article` synthesis per `(owner_group_id, wiki_key)`; every row for the
  key is its history, and a newer failed run leaves the previous article
  served. The registry inherits the syntheses' row security, so it adds no
  tenant table.
- **Generation:** the MCP tool `wiki_generate_article(theme_id,
  owner_group_id)`. It refuses, and writes nothing, when the theme has fewer
  than `MIN_READABLE_MEMBERS` (20) current members that the caller can read
  and the owner group may cite. It also refuses an owner group the caller
  cannot write, an unknown theme, and a theme without clustering provenance.
- **Read API for Phase C:** `GET /api/v1/eln/wiki` (every page the caller can
  read) and `GET /api/v1/eln/wiki/:group_id/:wiki_key` (the article, its
  `stale_since` and its version history). A malformed key, an unreadable
  group, or a page with no complete version all return 404, never an empty 200.
- **Group-scoped generation.** An article is a synthesis with
  `visibility = 'group'` and `owner_group_id = G`. Its row security shares it
  with G's members and no one else, so it needs no `synthesis_shares` row. It
  is generated by a **per-group curator agent** whose only group is G; see
  [`docs/runbooks/wiki-curator-agent.md`](../../runbooks/wiki-curator-agent.md).

#### Decisions taken for B (operator, 2026-10-07)

- **Per-group curator agent.** This resolves the first draft's open design
  item ("provision a generating agent per group, or pass a group viewer into
  the pipeline"). Generation runs as an agent that is a member of exactly one
  group (`group:main` first), so the synthesis reads with that group's scope.
  Provisioning the agent is an operator action, described in the runbook above;
  no code path creates the identity.
- **Registry = `syntheses` columns** (`wiki_key`, `seed_theme_id`) keyed on
  theme `properties`, not a `wiki_pages` table keyed on theme UUIDs:
  re-projection mints new theme ids.
- **"See also" moves to Phase C.** The Explorer can ask the kernel for related
  themes; episcience has no data for it.
- **"Contested" is LLM-reported until the real edge provider lands.**
  `episcience-worker` runs with `EmptyEdgeProvider`, so a production subgraph
  has no contradiction edges and the signed Louvain step has no negative ties
  to build the section from. An edge-backed Contested section waits for B-CKL
  Phase 4.

### C — Explorer `/wiki`

- `GET /wiki` lists the articles for the viewer's groups, grouped by group.
- `GET /wiki/{group}/{slug}` shows the article:
  - the rendered narrative, with each citation linked to `/claim/{id}` and its
    snapshotted belief;
  - the Contested section;
  - backlinks (other articles sharing member claims);
  - "See also";
  - a **stale since …** banner.
- Read-only, through the viewer's own token. A viewer outside the group gets
  404, never a blanked page. This follows the Explorer's existing BFF rules.
- Data: episcience's `GET /api/v1/eln/wiki` and
  `GET /api/v1/eln/wiki/:group_id/:wiki_key` (Phase B), read with the viewer's
  token. "See also" is computed here from the kernel's related themes (moved
  from B). Until the real edge provider lands, Contested is the section the
  article's LLM reported, not an edge-backed one.

### D — Nightly curation (EpiClaw)

- **A `wiki-curator` schedule, early morning PT.** Each run:
  1. picks **5 articles**: stale first, then never-written, in order of theme
     size, for `group:main`;
  2. runs the syntheses (`wiki_generate_article` with `owner_group_id`
     always named explicitly) as the group's curator agent, provisioned by
     [`docs/runbooks/wiki-curator-agent.md`](../../runbooks/wiki-curator-agent.md),
     and waits for each;
  3. replies with the Telegram summary: per article the title, claims cited,
     claims contested, what changed since the previous version, and failures.
- The scheduler delivers any reply other than `TASK_SILENT`. Reply
  `TASK_SILENT` only when there was nothing to do.
- **The schedule is a hand-deployed EpiClaw artifact** (`schedules.toml`). The
  deploy checklist must include it.
- **Backlog clearance takes weeks.** At 5 per night, about 200 top-level themes
  for one group take roughly six weeks to write once. Either accept that, or run
  a one-off bulk pass after the spike proves cost and reliability.

## Later

- **Hierarchical articles:** a sub-theme page under each top-level article.
- **Cross-theme insight:**
  - links between semantically related themes (centroid proximity in UMAP
    space);
  - cross-cluster relationships between atomic claims (supports / contradicts
    edges whose endpoints sit in different themes).

  These are a likely source of non-obvious findings and could become their own
  article type.
- **Markdown export** of a group's wiki (Obsidian-compatible) for offline
  reading.

## Risks

- **Visibility leakage.** This is the main risk: an article written with a
  broader scope than its readers'. It is mitigated by group-scoped generation
  and group-only sharing, and needs a test that a group's article cites no
  claim outside that group's read scope.
- **Theme churn.** If re-clustering renumbers themes, every article goes stale
  and URLs break. Stable theme ids (A.4) are a hard requirement.
- **Synthesis reliability.** The failure rate is unknown until the spike
  measures it.
