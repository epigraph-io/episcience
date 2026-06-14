# Design: Data-driven per-workflow verifier rubrics

**Status:** design-only. Single-repo (all in `episcience-core`) but medium-to-large
and judged premature. Touches the Phase-8 merge-gate verifier path, so the
correctness risk is non-trivial.

**Source:** "Out of scope" #4 of
`docs/superpowers/plans/2026-05-28-epiclaw-episcience-integration.md`.

---

## The load-bearing risk (read this first)

`SynthesisSkill::verify` is **the Phase-8 merge gate**. Phase-8 countersign
relies on its `VerificationOutcome`. A data-driven rubric introduces a parser and
an evaluator between the rubric author and the gate decision, and that layer has
two ways to be wrong, both worse than the status quo:

- **Fail-open:** a malformed / unparseable / empty rubric that evaluates to
  "pass" lets an un-cited or hallucinated-citation synthesis through the gate.
  This is the dangerous direction — it silently weakens the merge gate.
- **Over-reject:** a too-strict or mis-compiled rubric blocks valid syntheses,
  stalling the pipeline.

Today the rubric is Rust: a compile error is the failure mode, caught before
deploy. A data-driven rubric moves the failure mode to **runtime, per-workflow,
author-supplied** — exactly where it is hardest to catch. Any design here must
make **fail-closed the default**: an absent, invalid, or unparseable rubric must
reject (or fall back to the hard-coded `default_citation_rubric`), never pass.

---

## Current state (single-repo, `episcience-core`)

- `SynthesisSkill::verify` (`crates/episcience-core/src/synthesis/skill.rs`)
  defaults to `verifier::default_citation_rubric`
  (`crates/episcience-core/src/synthesis/verifier.rs` — regex citation
  discipline: every cluster member cited, no hallucinated citation).
- Skills override in code:
  - `code_review.rs` hand-writes a `code_review_pr_citation` rubric (regex:
    `#NNNN` must have a `[claim_id]` within 120 chars).
  - `literature.rs` and `registry_diff.rs` inherit the default.
- Skills are compile-time registered in `skills/mod.rs::load_by_name`.
- `VerificationOutcome` / `VerificationReason` are persisted (migration
  `5021_syntheses_verifier_outcome.sql`).

**Concrete gap:** no stored/configurable rubric. Adding or tuning a rubric
requires a Rust change + recompile + skill registration.

---

## Why premature

- Only **two** distinct rubrics exist (the default + `code_review_pr_citation`),
  and they differ by one regex parameter. A DSL to express two near-identical
  regex rubrics is a lot of machinery for a problem that is currently a
  one-line override.
- The deferral note in the plan is explicit: "interesting but premature."
- A third, genuinely different rubric is the trigger that would justify the DSL.
  Until then, the cost (DSL schema + parser + evaluator + per-workflow storage +
  rewiring every skill's `verify()`) exceeds the benefit.

---

## Sketch of the eventual shape (for when a third rubric arrives)

- **Storage:** a `verifier_rubric` JSONB column on the workflow row (not the
  synthesis row — the rubric is a property of the workflow definition, versioned
  with it).
- **Schema (constrained, not Turing-complete):** a small declarative shape —
  a list of named checks, each `{ kind: "every_member_cited" | "no_hallucinated_citation"
  | "pattern_proximity", params: {...} }`. Keep it an enum of vetted check kinds,
  **not** a free regex/eval surface; an arbitrary-regex rubric is a DoS and a
  fail-open surface.
- **Evaluator:** a pure function `evaluate(rubric, synthesis) -> VerificationOutcome`
  in `episcience-core`, total over all inputs, with an explicit
  `Err → reject` (fail-closed) policy.
- **Dispatch rewire:** `SynthesisSkill::verify` becomes
  "load rubric for this workflow; if present, evaluate; if absent or invalid,
  fall back to `default_citation_rubric`." The hard-coded skill rubrics
  (`code_review_pr_citation`) get expressed as seed rows in the new shape, and
  the Rust overrides are deleted only after the data-driven path is proven to
  reproduce them bit-for-bit.

---

## Test obligations (non-negotiable before any cutover)

1. **Fail-closed proof:** a malformed / empty / unknown-kind rubric yields
   `reject` (or the default rubric), never `pass`.
2. **Equivalence proof:** the data-driven evaluation of the seeded
   `code_review_pr_citation` and `default_citation_rubric` produces the
   identical `VerificationOutcome` to today's Rust implementations on a corpus of
   passing and failing syntheses. This is the gate that lets the Rust overrides
   be retired.
3. **No-regex-DoS proof:** the check kinds are a closed enum; no author-supplied
   pattern can be pathological against the verifier.

---

## Recommendation

Defer until a third skill needs a rubric the existing two cannot express. When
built, the design's center of gravity must be **fail-closed semantics + the
equivalence proof against the current Rust rubrics**, because this code is the
merge gate. The DSL ergonomics are secondary to not weakening Phase-8
countersign.
