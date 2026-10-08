---
name: wiki_article
description: One encyclopedic, cited article about one theme, for the
  group-scoped wiki — lede + headed sections around verbatim cluster
  blocks, LLM-reported Contested and Gaps sections, within-cluster
  de-duplication of repeated facts. No traversal opinion.
---

# Purpose

A wiki article is a synthesis (`skill_name = 'wiki_article'`,
`visibility = 'group'`) about one top-level theme. Its seeds are chosen
upstream: theme-anchored, viewer-filtered, near-duplicates suppressed and
selected for diversity (plan `2026-10-08-wiki-phase-b-articles.md`,
Tasks 1 and 3). This skill only shapes the prose.

# Overview

Write an encyclopedia article about one topic for readers who know
nothing about it. Neutral, factual tone; no first person.

# Narration

Summarise this cluster as one or two encyclopedic paragraphs. Cite every
claim as `[<claim_id>]`. If claims disagree or qualify each other, say so
explicitly and cite both sides. Within this cluster, state a fact once
even if several claims repeat it; cite all of them on that sentence.

# Composition

Write an encyclopedia article around the cluster blocks: a `# Title`
line, then a lede paragraph that defines the topic in its first sentence,
then the cluster blocks in a logical teaching order, each preceded by a
`##` heading you write. Your own text goes only between blocks; every
`<<<CLUSTER:{id}:BEGIN/END>>>` block stays verbatim, unchanged inside. If
any cluster reported disagreement, add `## Contested` after the last
block, listing each disagreement with citations to both sides. End with
`## Gaps` naming what the cited claims do not cover.

# Traversal

No override. The production worker runs with `EmptyEdgeProvider`, so an
article is exactly its seeds; a traversal opinion would be inert.

# Verification

Inherits the default citation rubric: every cluster member must be
cited; no citation may refer outside the cluster.

# Rationale (Phase 0 spike, 2026-10-07)

- **Repetition.** Near-duplicate claims (the same textbook fact across
  editions) were restated cluster after cluster. Seed-time suppression
  removes most of them; the narration instruction "state a fact once …
  cite all of them on that sentence" handles what remains inside a
  cluster while still satisfying the every-member-cited rubric.
- **Composition cannot de-duplicate across clusters.** `stage5_compose`
  requires each cluster summary byte-for-byte inside its sentinels and
  rejects any edit (`ComposeAnchorViolation`). The composition guidance
  therefore only adds text *between* blocks and never asks the composer
  to merge, reorder within, or reword a block.
- **Off-theme drift.** A label-only text seed cited 0 of its own theme's
  claims in one spike run; the theme-anchored seed (Task 3) fixes that
  upstream, which is why the overview names "one topic" rather than a
  free-text query.
- **No contradiction edges in production.** `## Contested` is
  LLM-reported from the narrations; an edge-backed Contested section
  waits for a real edge provider. "See also" moves to Phase C.
