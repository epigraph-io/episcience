---
name: code_review
description: Synthesis tuned for the nightly-bug-fix pipeline. PR-body-
  shaped narration; verifier adds a check that every #NNNN PR reference
  appears within 120 chars of a `[<claim_id>]` citation.
---

# Overview

Summarise a code-change run: which files changed, what invariants were
tested, which PRs opened.

# Narration

For each cluster, write a PR-body-shaped 3–5 sentence summary. Cite
every claim with `[<claim_id>]`. Cite PRs as `#<number>` and commits
as `` `<sha>` `` (7-char abbreviation acceptable). Do not invent any.

# Composition

Compose the per-cluster summaries into a Markdown narrative organised
as `## Summary` / `## Files changed` / `## Test plan` (standard PR
shape). Keep the `<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.

# Traversal

`max_hops=2`, `relevance_prune=0.6`, edge_types = Supports + Methodology.
Code changes follow shorter, denser citation trails than literature
scans.

# Verification

Two checks, in order:
1. Inherits the default citation rubric (every cluster member cited;
   no citation outside the cluster).
2. Adds: every `#<number>` PR reference must appear within 120 chars
   of a `[<claim_id>]` citation. Without this, PR mentions can drift
   into the narrative without a graph anchor — bad shape for a
   merge-gate review (Phase 8).
