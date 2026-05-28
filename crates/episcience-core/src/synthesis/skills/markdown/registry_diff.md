---
name: registry_diff
description: Synthesis tuned for the weekly-capability-audit workflow.
  Diff-shaped output (Added / Removed / Drifted tables); shallow
  Supersedes-only traversal.
---

# Overview

Summarise a capability-audit run: tools added, tools removed, tools
whose schemas drifted.

# Narration

For each cluster, list the capability changes it covers. Mark added
tools with `+`, removed with `-`, drifted with `~`. Cite every claim
with `[<claim_id>]`. Do not invent capability names.

# Composition

Compose the per-cluster summaries into a Markdown narrative organised
as three tables: `## Added` / `## Removed` / `## Drifted`. Each table
has columns: Tool, Version, Notes, `[<claim_id>]`. Keep the
`<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.

# Traversal

`max_hops=1`, `relevance_prune=0.6`, edge_types = Supersedes only.
Capability registry diffs follow version chains directly; depth > 1
brings in too much unrelated tooling.

# Verification

Inherits the default citation rubric (every cluster member cited;
no citation outside the cluster). A stricter "Removed claims must
carry epigraph_edge_id" check belongs at the review-bot tier — the
verifier here only sees narrative + member ids, not claim properties.
