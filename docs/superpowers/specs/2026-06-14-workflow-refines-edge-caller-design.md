# Design: REFINES `target_kind='workflow'` caller — linking workflow-generation chains to synthesis-refinement chains

**Status:** design-only for the **caller/linking** half. The **enablement** half
(the staging-table CHECK widening + round-trip test) SHIPPED in this branch
(`feat/b7-integration`): migration
`5031_synthesis_provo_edges_target_kind_workflow.sql` +
`crates/episcience-db/tests/synthesis_provo_edges_workflow_target_test.rs`.

**Source:** "Out of scope" #2 of
`docs/superpowers/plans/2026-05-28-epiclaw-episcience-integration.md`.

---

## What shipped vs. what is deferred

| Piece | State | Why |
|---|---|---|
| `target_kind='workflow'` accepted by the staging-table CHECK | **shipped** | additive, reversible, single-repo, low blast radius |
| Round-trip test (`plan` accepts `REFINES`/`workflow`; bogus kind still rejected) | **shipped** | non-tautological — the positive case failed on the CHECK pre-migration; the negative case proves the CHECK was *widened*, not *removed* |
| A caller that **emits** `REFINES target_kind='workflow'` edges | **deferred (this doc)** | "no caller" was the original deferral reason; the link semantics are the speculative part |
| Verifying EpiGraph `/edges` accepts `target_type='workflow'` | **deferred (this doc)** | cross-repo + unverified |

The enablement ships ahead of the caller deliberately: the shape is available the
moment a workflow↔refinement link is wired, but no dead edge is written until
there is a consumer for it.

---

## The cross-repo concern the migration does NOT resolve

`stage6_write_edges` (`crates/episcience-db/src/synthesis/publish.rs`) drains
pending `synthesis_provo_edges` rows and POSTs each to EpiGraph via `EdgeWriter`,
setting `target_type: edge.target_kind` directly:

```rust
let req = EdgeRequest {
    source_type: "synthesis".into(),
    source_id: synthesis_id,
    target_type: edge.target_kind.clone(),   // <- becomes "workflow"
    target_id: edge.target_id,
    relationship: edge.predicate.clone(),    // "REFINES"
};
edges_client.create_edge(req).await
```

So the staging-table CHECK is necessary but **not sufficient**: a planned
`workflow` edge will be POSTed to EpiGraph's edges service with
`target_type="workflow"`. **Whether EpiGraph's `/edges` API accepts
`target_type="workflow"` is unverified.** EpiGraph has workflow entities, so it
is *likely* yes — but this must be confirmed against the live EpiGraph edges
contract (or its OpenAPI / handler validation) before any caller is shipped.
Until confirmed, an emitted workflow edge would drain, fail the POST, and park in
`synthesis_provo_edges` with `record_failure` set (no data loss — the staging
table is the retry buffer — but the edge never lands).

**This verification is a prerequisite of the caller PR, not of the migration
PR.** The migration + test do not depend on it.

---

## Proposed caller semantics (when wired)

A synthesis-refinement chain (child refines parent synthesis) should be able to
declare that the refinement was *driven by* a workflow-generation chain — i.e.
the workflow that produced the work being synthesized. The edge:

- `source = synthesis` (the refinement child)
- `predicate = REFINES`
- `target_kind = workflow`, `target_id = <workflow node id>`

This reuses the exact `REFINES` predicate already emitted for
synthesis→synthesis refinement (`stage6_plan_edges` in `publish.rs`), extending
its range to include workflow nodes. The plumbing
(`SynthesisProvoEdgesRepository::plan` / `list_pending` / `mark_written`) is
already `target_kind`-agnostic, so the caller only needs to construct the
`ProvenanceEdge` and hand it to `plan` alongside the existing edges.

### Where the caller would live

In `crates/episcience-api/src/jobs/synthesis_job.rs`, on the
refinement-child path (the same place the existing synthesis→synthesis `REFINES`
edge is inserted), *if* the originating request carries a `workflow_id`
correlation key. That key is the same one Item 1 (the synthesis-ready push) wants
threaded through the enqueue → job. **The two deferred items share a correlation
key**: wiring `workflow_id` through once unblocks both.

---

## Open questions

1. EpiGraph `/edges` acceptance of `target_type="workflow"` (must verify — see
   above).
2. Source of the `workflow_id` on the synthesis request. The EpiClaw enqueue
   knows the originating `workflow_run`; this must be threaded through
   `EpiscienceClient` → synthesis job → edge. (Shared with Item 1.)
3. Direction/semantics: is `REFINES synthesis → workflow` the right arrow, or
   should it be `WAS_DERIVED_FROM`? `REFINES` matches the existing
   synthesis-refinement vocabulary, but the link is arguably derivation, not
   refinement. Decide with the EpiGraph edge-type owner before emitting.

---

## Recommendation

Hold the caller until (a) a concrete consumer wants workflow↔refinement links in
the graph, and (b) EpiGraph `/edges` is confirmed to accept
`target_type="workflow"`. The enablement is in place, so the caller is then a
small, contained addition. Co-schedule with Item 1, since both need the same
`workflow_id` correlation key threaded through the synthesis job.
