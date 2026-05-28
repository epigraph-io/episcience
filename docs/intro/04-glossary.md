# Glossary (science-specific)

Vocabulary for episcience's experimental loop layer. For kernel terms (claim, edge, agent, BetP, etc.) see the [EpiGraph glossary](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/04-glossary.md).

---

## baseline skill

The default [synthesis skill](#skill-synthesis) — the registered Rust impl behind `crate::synthesis::skills::baseline::BaselineSkill`. Encodes the pre-skill pipeline's inline prompts for the `Overview`, `Narration`, `Composition`, and `Verification` stages and provides the citation-discipline default for `verify`. Loaded when a synthesis row's `skill_name` is `"baseline"` (the default value) or when an unknown skill name forces fallback. Behaviour-preserving — a synthesis run through `baseline` produces byte-identical prompts to a pre-refactor run. [see 02-concepts-science.md §7 (Synthesis skills)](02-concepts-science.md#7--synthesis-skills)

## blob

A content-addressed binary artifact — gel image, microscope frame, instrument trace, attached PDF — stored on the filesystem at `EPISCIENCE_BLOB_DIR/{hash[0:2]}/{hash[2:4]}/{hash}.blob` with metadata (filename, MIME type, size, uploader, optional `sample_id`) in the `blobs` table. Duplicate uploads are deduplicated by BLAKE3 `content_hash`, so the same raw image referenced from two experiments stores one copy. Blobs are how raw data enters the experimental loop; they are typically referenced by the synthesis claims or experiment results that interpret them. [see 02-concepts-science.md §4 (Blobs)](02-concepts-science.md#4--blobs)

## blob uploader (EpiClaw)

The EpiClaw-side scanner — Rust type `BlobUploader` in `epiclaw-host/src/host/blob_uploader.rs` — that walks `/workspace/group/<group>/` for files modified during a scheduled task's run and `POST /api/v1/eln/blobs` each one as a multipart upload tied to the task's [workflow_run sample](#workflow_run-sample). Caps: 50 files per run, 50 MB per file; oversize files are logged at warn and skipped. Fire-and-forget per file — a failed upload does not abort the rest. Shipped in epiclaw-host PR #17 (Phase 7).

## code_review skill

The PR-body-shaped [synthesis skill](#skill-synthesis) registered as `crate::synthesis::skills::code_review::CodeReviewSkill` (`skill_name = "code_review"`). Overrides `Narration` (3-5 sentence summaries, `[<claim_id>]` citations, `#<number>` for PRs, `` `<sha>` `` for commits), `Composition` (Markdown organised as `## Summary` / `## Files changed` / `## Test plan`), `traversal_config` (`max_hops = 2`, `Supports + Methodology`), and `verify` (after the default citation rubric, every `#NNNN` mentioned must have a `[<claim_id>]` citation within 120 characters — rubric name `code_review_pr_citation`). The strictness exists because the narrative may become a merge gate (see [countersign-as-merge-gate](#countersign-as-merge-gate)). [see 02-concepts-science.md §13.2 (CodeReviewSkill)](02-concepts-science.md#132--codereviewskill-code_review)

## countersign-as-merge-gate

The operator recipe in which a downstream merge step (e.g. `gh pr ready`) refuses to proceed until the synthesis narrative carries at least one [countersignature](#countersignature) with `signature_meaning = "approved"` from an agent other than the synthesis's author. The check is a `list_countersignatures` call (HTTP or MCP) on the narrative's `claim_id` followed by a `signer_id`-distinct count. Approval-shaped only — there is no rejected `signature_meaning`; a failed gate is simply the absence of an approved row. The full recipe (review-bot agent, scheduling, gate command) lives in [`05-workflows.md` Workflow E](05-workflows.md#workflow-e--countersign-as-merge-gate-review-bot).

## countersignature

A second agent's Ed25519 signature attesting to an existing claim with one of five meanings: `witnessed`, `approved`, `reviewed`, `certified`, or `countersigned`. Each row pins `claim_id`, `signer_id`, the 32-byte BLAKE3 `content_hash` it signed, the 64-byte signature, and (since migration 5010) a `prev_signature_hash` that chains countersignatures for tamper-evident review trails. Countersignatures are how the lab ELN layer encodes the "two-person rule" — a synthesis claim or measurement is not "approved" until a second qualified agent has signed it. [see 02-concepts-science.md §5 (Countersignatures)](02-concepts-science.md#5--countersignatures)

## experiment

The run-time instantiation of a protocol against one or more samples to test a hypothesis — conceptually, a single execution of "apply protocol P to sample S and observe outcome O." A schema exists (`experiments` table, extracted from EpiGraphV2 migration 049) with `hypothesis_id`, `protocol`, `status` (`designed → running → collecting → analyzing → complete | failed`), and timestamps, but there is no `experiments` API route in episcience today. In practice, the user-facing surface uses synthesis claims that reference sample and protocol IDs; an `experiments` endpoint is planned but not present. [see 02-concepts-science.md §1 (Experiments and experiment-results)](02-concepts-science.md#1--experiments-and-experiment-results)

## experiment-result

The observed outcome of an experiment — the bridge from raw measurements to a claim that propagates belief. The `experiment_results` table records `experiment_id`, `data_source` (`manual | simulation | instrument | literature | computed`), `raw_measurements` (JSONB), `measurement_count`, `effective_random_error`, and `processed_data`, all linked back to the parent experiment via `ON DELETE CASCADE`. As with `experiment`, the surface today is synthesis claims referencing samples and blobs rather than a dedicated `experiment_results` route. [see 02-concepts-science.md §1 (Experiments and experiment-results)](02-concepts-science.md#1--experiments-and-experiment-results)

## lab_notebook skill

The ELN-tuned [synthesis skill](#skill-synthesis) registered as `crate::synthesis::skills::lab_notebook::LabNotebookSkill`. Overrides the baseline in three ways: `Narration` asks for chronological 2–4-sentence summaries that mention protocols and samples by id; `Composition` produces an oldest-first chronological Markdown narrative; and `traversal_config` narrows to `max_hops = 2`, `edge_types = [Supports, Corroborates]`, `relevance_prune = 0.55` — deliberately excluding argumentative or methodological edges. Selected via `skill_name: "lab_notebook"` on `POST /eln/syntheses`. [see 02-concepts-science.md §7 (Synthesis skills)](02-concepts-science.md#7--synthesis-skills)

## literature skill

The arxiv-scan-tuned [synthesis skill](#skill-synthesis) registered as `crate::synthesis::skills::literature::LiteratureSkill` (`skill_name = "literature"`). Overrides `Narration` (every claim cited with `[<claim_id>]` AND its DOI in parentheses — `(doi:10.xxx/yyy)` or `(arxiv:NNNN.NNNNN)` when no DOI exists), `Composition` (ordered by methodology family then publication date), and `traversal_config` (`max_hops = 3` over `Supports + Methodology + Corroborates` — the widest reach of any shipped skill, justified by the recall demands of literature scanning). Verification inherits the default citation rubric — the literature-specific quality signal is novelty, dispatched to [paper-novelty backend](#paper-novelty-backend). [see 02-concepts-science.md §13.1 (LiteratureSkill)](02-concepts-science.md#131--literatureskill-literature)

## novelty backend

The pluggable scorer behind Stage 7 of the synthesis pipeline. Implementations satisfy the `NoveltyBackend` trait (`name() + async score(...)`) and produce a [novelty score](#novelty-score) for a candidate synthesis. The default backend is `InternalNoveltyBackend` (name `"internal_prior_syntheses"`), which scores against prior `complete` syntheses by `0.5 * cosine(narrative_embedding) + 0.5 * jaccard(member_ids)`. Future backends can plug in external corpora (PubMed, arXiv, etc.) without touching the pipeline shape. [see 02-concepts-science.md §9 (Novelty assessment)](02-concepts-science.md#9--novelty-assessment)

## novelty score

The result of Stage 7 of the synthesis pipeline — a 0.0 (fully redundant) to 1.0 (highly novel) measure plus structured neighbour evidence (top-5 prior syntheses by similarity) and a free-form rationale. Persisted on the `syntheses` row as `novelty_score JSONB` + `novelty_backend TEXT`. Computed once at acceptance time; never recomputed. Novelty failures are non-fatal — the synthesis still completes with `novelty_score = NULL` if the backend errors. [see 02-concepts-science.md §9 (Novelty assessment)](02-concepts-science.md#9--novelty-assessment)

## paper-novelty backend

The literature-tuned [novelty backend](#novelty-backend) registered as `crate::synthesis::novelty_backend_paper::PaperNoveltyBackend` (`name = "paper_novelty"`). Extends the internal-priors backend by additionally scoring the candidate against prior `doi`-labeled kernel `claims`: `score = min(internal_score, 1.0 - top_doi_similarity)`. Dispatched at job-pickup when `skill_name == "literature"`; every other skill falls back to `InternalNoveltyBackend`. Empty-corpus property: when no DOI-labeled claims exist (common today), `top_doi_similarity = 0.0` and the combined score collapses to the internal score — no behaviour break against a fresh kernel. [see 02-concepts-science.md §14 (Paper-novelty backend)](02-concepts-science.md#14--paper-novelty-backend)

## PROV-O

The W3C Provenance Ontology — a standard vocabulary for describing how things came to be. The `synthesis_provo_edges` table allows four predicates: `WAS_DERIVED_FROM` (the only one taken directly from the W3C PROV-O standard, corresponding to `wasDerivedFrom`), plus episcience-specific predicates `REFINES`, `COMPOSED_OF`, and `ATTRIBUTED_TO` (the last is inspired by PROV-O's attribution concept but is not a standard PROV-O relation as named). These are kept in a separate table from the kernel's epistemic edge types (supports, refutes, refines, etc.) so dependency provenance does not conflate with belief-bearing edges. [see 02-concepts-science.md §6 (Synthesis claims and PROV-O edges)](02-concepts-science.md#6--synthesis-claims-and-prov-o-edges)

## protocol

A versioned, traceable lab SOP — the recipe an experiment instantiates. The `protocols` table stores `title`, integer `version`, an ordered `steps` JSONB array (each step may carry duration, temperature, notes), an `equipment` list, optional `safety_notes`, and a `supersedes` self-reference for the version chain; a BLAKE3 `content_hash` over the steps lets clients detect drift. Every experiment must reference the specific protocol version used, so a later edit produces a new protocol rather than mutating the cited one. Since migration 5025, protocols also carry an additive `sections` column — see [skill section](#skill-section). [see 02-concepts-science.md §3 (Protocols)](02-concepts-science.md#3--protocols)

## refinement chain

A parent → child sequence of synthesis rows produced by simulated-annealing recovery from a verifier reject. Each child inherits the parent's `skill_name`, carries an annealed [refinement temperature](#refinement-temperature), and is linked back via a `synthesis_provo_edges` row of predicate `REFINES`. The parent stays in `rejected`; the child is the live attempt. Maximum chain length is 4 (root + three refinements at `depth_delta` 1, 2, 3). [see 02-concepts-science.md §10 (Refinement chains)](02-concepts-science.md#10--refinement-chains)

## refinement temperature

The simulated-annealing knob carried on each synthesis row as `refinement_temperature JSONB`. Three fields: `depth_delta: u8` (hops added on top of the parent's traversal config, bounded at 3), `relevance_prune_relax: f32` (multiplier on `relevance_prune`, floored at 0.4), and `allow_soft_verifier: bool` (true after the first reject; forward-compatible signal for skill rubrics that may downgrade strict checks). The `anneal()` method advances one step; `at_ceiling()` returns true at `depth_delta >= 3`. Default JSON: `{"depth_delta":0,"relevance_prune_relax":1.0,"allow_soft_verifier":false}`. [see 02-concepts-science.md §10 (Refinement chains)](02-concepts-science.md#10--refinement-chains)

## REFINES edge

A `synthesis_provo_edges` row of predicate `REFINES` pointing from a child synthesis to its parent. Distinct from the kernel's `supports`/`refutes`/`refines` epistemic edges (which live in a separate table and propagate belief): a PROV-O `REFINES` edge expresses *the child was generated as a refinement attempt of the parent*, not a belief relation. Required for traversing a [refinement chain](#refinement-chain). [see 02-concepts-science.md §10 (Refinement chains)](02-concepts-science.md#10--refinement-chains)

## registry_diff skill

The capability-audit-tuned [synthesis skill](#skill-synthesis) registered as `crate::synthesis::skills::registry_diff::RegistryDiffSkill` (`skill_name = "registry_diff"`). Overrides `Narration` (capability changes marked `+` added, `-` removed, `~` drifted, each `[<claim_id>]`-cited), `Composition` (three Markdown tables — `## Added` / `## Removed` / `## Drifted` — with Tool / Version / Notes / `[<claim_id>]` columns), and `traversal_config` (`max_hops = 1`, `Supersedes`-only — the shallowest reach of any shipped skill, justified because tool versions chain through `Supersedes` and a one-hop traversal stays precise). Verification inherits the default citation rubric. [see 02-concepts-science.md §13.3 (RegistryDiffSkill)](02-concepts-science.md#133--registrydiffskill-registry_diff)

## review-bot

The operator-built Claude agent that drives a [countersign-as-merge-gate](#countersign-as-merge-gate) workflow — a recurring EpiClaw scheduled task that polls `list_syntheses(skill_name=...)` for unsigned candidates, re-runs the relevant skill's rubric locally on each narrative, and on accept calls `countersign(claim_id, signature_meaning="approved", ...)` with a distinct Ed25519 keypair. The bot is not shipped code; episcience exposes the read-side tools the bot needs (see [02-concepts-science.md §15](02-concepts-science.md#15--review-bot-read-side-tooling)) and EpiClaw provides scheduling + agent identity. Full recipe: [`05-workflows.md` Workflow E](05-workflows.md#workflow-e--countersign-as-merge-gate-review-bot).

## sample

A tracked physical material — DNA origami batch, protein construct, substrate, reagent, aliquot — with chain-of-custody from preparation through disposal. The `samples` table tracks `sample_type`, a `status` lifecycle (`prepared → in_use → consumed | disposed → archived`), `parent_sample_id` for aliquots and derivatives, `prepared_by`, quantity, storage location, hazard info, and a BLAKE3 `content_hash`; the `sample_claims` junction links a sample to EpiGraph claims as `observation`, `measurement`, `characterization`, or `preparation_note`. Samples are how the physical world enters the graph: nothing is observed without a sample to observe it from. [see 02-concepts-science.md §2 (Samples)](02-concepts-science.md#2--samples)

## skill (synthesis)

A pluggable specialisation of the synthesis pipeline. Implements the `SynthesisSkill` trait (`name`, `section`, `traversal_config`, `verify`) — together those four methods control which prompt strings are spliced into which [synthesis stage](#synthesis-stage), what traversal config is used as the default, and what rubric the verifier applies. Two skills ship today: [baseline](#baseline-skill) and [lab_notebook](#lab_notebook-skill). The active skill per-synthesis is named in `syntheses.skill_name`. [see 02-concepts-science.md §7 (Synthesis skills)](02-concepts-science.md#7--synthesis-skills)

## skill registry

The compile-time table of registered synthesis skills behind `crate::synthesis::skills::load_by_name`. Adding a new skill is two file edits (the impl + the lookup arm) plus a migration that extends the `syntheses_skill_name_known` CHECK constraint to allow the new name. Writes are strict: an unknown name on `POST /eln/syntheses` fails at the DB CHECK rather than silently falling back. Reads tolerate unknowns (fallback to baseline + warning) so stale rows do not block the worker. [see 02-concepts-science.md §7 (Synthesis skills)](02-concepts-science.md#7--synthesis-skills)

## skill section

A skill-provided prompt fragment that the synthesis pipeline splices into a known [synthesis stage](#synthesis-stage). Returned by `SynthesisSkill::section(stage)` as `Option<&str>`; `None` means "fall back to the pipeline's baseline prompt for that stage." Short, focused content belongs here; multi-paragraph guidance belongs in the sibling markdown reference under `crates/episcience-core/src/synthesis/skills/markdown/`. The parallel concept on the protocol side is the structured `sections` column on the `protocols` table (overview / planning / implementation / interpretation / validation, plus `extras` for off-vocab keys) — see [02-concepts-science.md §11](02-concepts-science.md#11--protocol-section-vocabulary).

## synthesis claim

A narrative claim generated by clustering a subgraph and asking an LLM to summarize it — episcience's current stand-in for "experiment result" in user-facing flows. A `syntheses` row pins the originating `query`, the captured `subgraph_snapshot` (claim IDs, edge IDs, belief intervals, traversal config), the clustering method (`signed_louvain`), LLM provider/model, the generated `narrative`, a BLAKE3 `content_hash`, and a staleness field that can fire on `belief_drift`, `new_contradiction`, `claim_superseded`, `frame_changed`, or `edge_revoked`. Synthesis claims are linked back into the kernel via `synthesis_provo_edges` (PROV-O) and via membership edges to the claims they summarize. [see 02-concepts-science.md §6 (Synthesis claims and PROV-O edges)](02-concepts-science.md#6--synthesis-claims-and-prov-o-edges)

## synthesis_skill (task field)

The opt-in field on an EpiClaw `ScheduledTask` (in `schedules.toml` or via the IPC schedule API) naming the episcience [skill](#skill-synthesis) to enqueue on successful task completion. When set alongside a `workflow_id`, the host's [workflow run hook](#workflow-run-hook-epiclaw) calls `POST /api/v1/eln/syntheses` with `skill_name = <value>` after the container exits. When unset, the synthesis enqueue is skipped (sample + observation + blobs are still recorded if `EPISCIENCE_URL`/`EPISCIENCE_BEARER` are configured). The string maps 1:1 to episcience's `syntheses.skill_name` column. See `epiclaw-host/docs/integration-with-episcience.md`.

## synthesis stage

A variant of the closed `SynthesisStage` enum — the eight-element section vocabulary the synthesis pipeline knows how to splice [skill sections](#skill-section) into: `Overview`, `Planning`, `Traversal`, `Clustering`, `Narration`, `Composition`, `Verification`, `Novelty`. Adding a new stage is a deliberate pipeline change (a new variant + new call site in the pipeline); skills cannot invent stages. The naming mirrors SciLink's foundation-agent set, extended with the stages specific to graph-clustering synthesis. [see 02-concepts-science.md §7 (Synthesis skills)](02-concepts-science.md#7--synthesis-skills)

## task sections (EpiClaw schedules.toml)

The optional `[schedules.sections]` TOML block on an EpiClaw scheduled task that supplies a structured prompt aligned with episcience's [skill section](#skill-section) vocabulary — `overview`, `planning`, `implementation`, `interpretation`, `validation`, plus a free-form `extras` map for off-vocab keys. Rust type: `TaskSections` in `epiclaw-host/src/host/task_sections.rs`. When present and non-empty, the prompt sent to the container is rendered as `# OVERVIEW / # PLANNING / # IMPLEMENTATION / # INTERPRETATION / # VALIDATION / # EXTRAS` sections in that fixed order; the legacy flat `prompt` field fills in as the implementation section when `sections.implementation` is unset. Existing schedules.toml entries with just `prompt = "..."` parse and behave unchanged.

## verification outcome

The serde-tagged result of running a skill's [verification rubric](#verification-rubric) — `VerificationOutcome::Accept { rubric, evidence }` or `VerificationOutcome::Reject { rubric, reason, evidence }`. Reject reasons are `UncitedMember`, `HallucinatedCitation`, `KernelContradiction` (reserved), or `SkillRejection`. Persisted on every Stage-6 run as `syntheses.verifier_outcome JSONB` alongside `verifier_attempts SMALLINT`. Accept gates the row to `status = 'complete'`; Reject routes to refinement (§10) or terminates the row in `status = 'rejected'`. [see 02-concepts-science.md §8 (Verifier-driven acceptance)](02-concepts-science.md#8--verifier-driven-acceptance)

## verification rubric

The acceptance policy applied at Stage 6 of the synthesis pipeline. Default rubric (`default_citation_rubric` in `crates/episcience-core/src/synthesis/verifier.rs`): every cluster member must appear as a `[<claim_id>]` citation, and no citation may refer to a claim outside the cluster. Skills override `SynthesisSkill::verify` to add stricter checks; the rubric name (e.g. `"default_citation"`) is recorded in the [verification outcome](#verification-outcome) so post-hoc inspection can tell which rubric ran. [see 02-concepts-science.md §8 (Verifier-driven acceptance)](02-concepts-science.md#8--verifier-driven-acceptance)

## workflow run hook (EpiClaw)

The EpiClaw-side post-task wiring — Rust type `WorkflowRunHook` in `epiclaw-host/src/host/workflow_run_hook.rs` — that fires after a successful container exit when both `workflow_id` and [`synthesis_skill`](#synthesis_skill-task-field) are set on the task. Sequence: `POST /api/v1/eln/workflow_runs` (creates the [workflow_run sample](#workflow_run-sample)), then `POST /api/v1/eln/samples/:id/observations` for the task output, then `POST /api/v1/eln/syntheses` with the named skill. Fire-and-forget — failures at any step are logged at `warn` level (`episcience workflow_run create failed (non-fatal)` / `episcience synthesis enqueue failed (non-fatal)`) but never propagated to the scheduler. Shipped in epiclaw-host PRs #15 (synthesis), #16 (observation), #17 (blobs).

## workflow_run sample

A `samples` row whose `sample_type` is the literal string `workflow_run`, recording an out-of-band orchestrator's (today: EpiClaw) execution of a named EpiGraph workflow. The `name` is the workflow's `canonical_name`; `properties.workflow_id` carries the EpiGraph workflow UUID; `preparation_date` is the run's `started_at`; the literal `"workflow_run"` also appears in `labels` for cheap label-only queries. Created via the dedicated `POST /api/v1/eln/workflow_runs` route (which honours the caller's `started_at` rather than stamping `NOW()`); CHECK-constrained via migration 5026. Once created, the row behaves like any other sample — observations, blobs, countersignatures attach normally. [see 02-concepts-science.md §12 (Workflow runs)](02-concepts-science.md#12--workflow-runs)
