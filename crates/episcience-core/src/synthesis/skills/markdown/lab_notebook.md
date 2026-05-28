---
name: lab_notebook
description: Synthesis tuned for ELN narrative summaries — chronological
  ordering, protocol and sample citation, narrower traversal.
---

# Overview

Synthesizes an experimental-loop slice into a chronological narrative
suitable for inclusion in a lab notebook entry. Cites protocols and
samples by id alongside claims.

# Narration

For each cluster, write a chronological 2–4 sentence summary mentioning
the protocol used and the samples observed. Cite every claim with
`[<claim_id>]`. Cite protocols as `(protocol:<title>@v<version>)` and
samples as `(sample:<name>)` when relevant. Do not invent any.

# Composition

Compose the per-cluster summaries into a chronologically ordered
Markdown narrative (oldest first). Keep the
`<<<CLUSTER:{id}:BEGIN/END>>>` sentinels verbatim.

# Traversal

Narrow to `max_hops=2`, `relevance_prune=0.55`, and only the
observational-lineage edge types. Lab-notebook synthesis prefers
high-signal lineage over wide thematic coverage.
