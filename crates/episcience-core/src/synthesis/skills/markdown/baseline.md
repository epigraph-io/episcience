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
