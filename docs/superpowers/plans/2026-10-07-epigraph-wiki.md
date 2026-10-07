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

### B — Articles in episcience

- **`WikiArticleSkill`**, a `SynthesisSkill` that writes an encyclopedic
  article: a lede, sections by sub-cluster, a "Contested" section built from the
  negative (contradiction) ties of the signed Louvain step, and "See also" from
  related themes.
- **A theme-anchored seed.** A synthesis today starts from a text query. Add a
  seed of "the members of theme T readable by group G" so an article cannot
  drift off its theme.
- **A `wiki_pages` table** keyed by `(group_id, theme_id)`, holding the slug,
  current synthesis id, status, `generated_at` and `superseded_synthesis_ids`
  for history. An article exists only when the group can read at least N claims
  in the theme.
- **Group-scoped generation.** The synthesis must read with exactly the group's
  scope (public claims plus that group's own) and be shared only with that
  group via `synthesis_shares`. **Open design item:** today a synthesis runs as
  the calling agent. Either provision a generating agent per group, or pass a
  group viewer into the pipeline. Decide this before writing B's task plan.

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

### D — Nightly curation (EpiClaw)

- **A `wiki-curator` schedule, early morning PT.** Each run:
  1. picks **5 articles**: stale first, then never-written, in order of theme
     size, for `group:main`;
  2. runs the syntheses and waits for each;
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
