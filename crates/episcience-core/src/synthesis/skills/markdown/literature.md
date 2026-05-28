---
name: literature
description: Synthesis tuned for arxiv research-scan workflows — DOI
  and arxiv citation formatting, methodology-grouped composition,
  wider 3-hop traversal across Supports + Methodology + Corroborates
  edges.
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

`max_hops=3`, `relevance_prune=0.5`, edge_types = Supports + Methodology
+ Corroborates. Literature work follows wider citation trails than ELN
narratives, so the default depth is loosened.

# Verification

Inherits the default citation rubric: every cluster member must be
cited; no citation may refer outside the cluster.
