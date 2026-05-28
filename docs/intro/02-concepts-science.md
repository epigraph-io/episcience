# Science-layer concepts

Episcience extends the [EpiGraph kernel](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) (claims, edges, agents, signatures, frames) with the run-time furniture of an experimental loop: samples to observe, protocols to run, blobs to capture, countersignatures to attest, and synthesis claims to summarize. Each of these is a thin add-on table that references kernel rows by id — no kernel concept is replaced. Where the kernel records *what is believed and why*, the science layer records *what was done, with what, by whom, and what was produced*.

This document walks the six science-layer concepts in the order an experiment touches them.

## Contents

1. [Experiments and experiment-results](#1--experiments-and-experiment-results)
2. [Samples](#2--samples)
3. [Protocols](#3--protocols)
4. [Blobs](#4--blobs)
5. [Countersignatures](#5--countersignatures)
6. [Synthesis claims and PROV-O edges](#6--synthesis-claims-and-prov-o-edges)
7. [Synthesis skills](#7--synthesis-skills)
8. [Verifier-driven acceptance](#8--verifier-driven-acceptance)
9. [Novelty assessment](#9--novelty-assessment)
10. [Refinement chains](#10--refinement-chains)
11. [Protocol section vocabulary](#11--protocol-section-vocabulary)
12. [Workflow runs](#12--workflow-runs)
13. [Per-workflow synthesis skills](#13--per-workflow-synthesis-skills)
14. [Paper-novelty backend](#14--paper-novelty-backend)
15. [Review-bot read-side tooling](#15--review-bot-read-side-tooling)

---

## 1.  Experiments and experiment-results

This builds on the kernel concept of *claim* — see the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

Conceptually, an *experiment* is the run-time instantiation of a protocol against one or more samples: "apply protocol P to sample S and observe outcome O." An *experiment-result* is the observed outcome, recorded as a kernel claim that other claims can later support, refute, or refine. In a fully expanded ELN this would be a first-class row with a status lifecycle (`designed → running → collecting → analyzing → complete | failed`), a hypothesis pointer, a protocol pointer, and a child `experiment_results` row that carries the actual data.

**Caveat — there is no `experiments` route on the current episcience surface, even though the tables exist.** Migration `001_initial_schema.sql` does create `experiments` (with `hypothesis_id`, `created_by`, `method_ids`, `protocol`, `protocol_source`, `status` in `designed | running | collecting | analyzing | complete | failed`, `started_at`, `completed_at`) and `experiment_results` (with `experiment_id`, `data_source` in `manual | simulation | instrument | literature | computed`, `raw_measurements`, `measurement_count`, `effective_random_error`, `processed_data`, status, linked back via `ON DELETE CASCADE`). What is missing is an Axum router exposing them — no `POST /experiments` or `POST /experiment_results` exists. Today, an experiment is reconstructed from three pieces:

- a kernel **claim** whose content describes the hypothesis or observation,
- one or more **sample_claims** rows tying the claim to the materials observed (`relationship` in `observation | measurement | characterization | preparation_note`),
- an optional **synthesis** that later narrates the cluster of claims arising from the run.

The protocol used is referenced by convention in claim content, labels, or properties until a dedicated `experiments` write endpoint lands. This is enough to reconstruct an experiment for auditing — *who prepared the sample, who observed it, against which protocol version* — but it is not yet enough to enforce the relationships at write time. In particular, nothing today stops a claim from citing a `sample` that has been `disposed`, or a `protocol` that was later superseded.

The closest concrete write path is `POST /api/v1/eln/samples/:id/observations`, which atomically inserts a claim plus a `sample_claims` row tying the new claim to the sample. The `relationship` defaults to `observation`; pass `measurement`, `characterization`, or `preparation_note` to refine. The handler enforces `auth.agent_id == agent_id` so observations are always self-attested:

```json
{
  "content": "AFM scan of batch 12 shows 87% well-formed tiles.",
  "agent_id": "0193a2c1-...-agent-uuid",
  "relationship": "measurement"
}
```

The response carries the new `claim_id`, the `sample_id` the observation is tied to, and the relationship. The kernel-side belief on that claim defaults to a neutral starting point; later edges from peer claims, refutations, or syntheses will move BetP up or down as evidence accumulates.

A future `experiments` endpoint is on the roadmap. When it ships it will be additive: the tables are already there, so the new endpoint will write `experiments` and `experiment_results` rows alongside the same claim — likely with a `synthesis_provo_edges` row of predicate `WAS_DERIVED_FROM` pointing from any downstream synthesis back to the experiment row. Existing claim + sample_claim records will keep working unchanged.

In the meantime, the recommended pattern is: prepare the sample with `POST /api/v1/eln/samples`, attach raw data with blob uploads (see §4), file each observation via `POST /api/v1/eln/samples/:id/observations`, and then drive a synthesis (see §6) when enough observations have accumulated to merit narration. Every step is a kernel-claim write, so kernel tools — recall, BetP, frame propagation — work end-to-end without an `experiments`-aware client.

This is intentionally a thin layer: when the `experiments` route lands, no existing data needs to be migrated. The schema is already in place; only the HTTP surface and worker plumbing need to be added. Any tool that has already been reading `sample_claims` rows will keep working; the new endpoint just gives writers a structurally enforced place to record the protocol-and-status side of the loop.

**See also:**
- `experiments` + `experiment_results` schema: [`migrations/001_initial_schema.sql`](../../migrations/001_initial_schema.sql)
- Sample-claim junction: [`migrations/5003_create_samples.sql`](../../migrations/5003_create_samples.sql) (table `sample_claims`)
- Observation creation: [`crates/episcience-api/src/routes/samples.rs`](../../crates/episcience-api/src/routes/samples.rs) (`add_observation`, `POST /api/v1/eln/samples/:id/observations`)
- Glossary entries: [experiment](04-glossary.md#experiment), [experiment-result](04-glossary.md#experiment-result)

---

## 2.  Samples

This builds on the kernel concept of *claim* (via the `sample_claims` junction) — see the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

A *sample* is a tracked physical or digital artifact — DNA origami batch, protein construct, substrate, reagent, aliquot, dataset file. The `samples` table pins identity (`name`, `sample_type`), lifecycle (`status` in `prepared | in_use | consumed | disposed | archived`), provenance (`prepared_by`, `preparation_date`, optional `expiry_date`, `storage_location`), quantity (`quantity_value` + `quantity_unit`), domain metadata (`hazard_info`, `properties` JSONB, `labels` text array), and a 32-byte BLAKE3 `content_hash` for integrity. The `sample_claims` junction is how observations enter the kernel: each row carries a `relationship` of `observation | measurement | characterization | preparation_note`, so a downstream reader can tell whether a claim is "I saw this" or "I measured this to be X" or "I noted this during prep."

Samples can have parent samples — an aliquot, a derivative, a fraction — via `parent_sample_id`. Migration `5009_samples_parent_restrict.sql` changes the parent FK from `ON DELETE SET NULL` to `ON DELETE RESTRICT`, so you cannot delete a sample that still has children — the lineage is preserved or the deletion fails. (Circular lineage is structurally prevented because the FK requires the parent to exist before the child is inserted; you cannot retroactively cycle the chain without first violating that constraint.) The status check constraint enforces a narrow set; transitions between them are validated server-side by `SampleStatus::can_transition_to`, so the database holds the spelling and the API holds the lifecycle.

A `POST /api/v1/eln/samples` request, grounded in `CreateSampleRequest`:

```json
{
  "name": "origami-tile-batch-12",
  "sample_type": "dna_origami",
  "prepared_by": "0193a2c1-...-agent-uuid",
  "parent_sample_id": "0193a2b8-...-parent-uuid",
  "storage_location": "freezer-3/shelf-2/box-A",
  "quantity_value": 50.0,
  "quantity_unit": "uL",
  "hazard_info": {},
  "labels": ["batch-12", "checkpoint-A"],
  "properties": {"concentration_nM": 200, "buffer": "TAE-Mg"}
}
```

The handler enforces that `auth.agent_id == prepared_by` (no third-party preparation), parses `sample_type` and any quantity, computes a content hash over `name:sample_type:prepared_by`, and returns the persisted `Sample` row. Status transitions are validated server-side by `SampleStatus::can_transition_to` via `PATCH /api/v1/eln/samples/:id/status`:

```json
{ "status": "in_use" }
```

The transition `prepared → in_use → consumed | disposed → archived` is the canonical happy path; illegal jumps (e.g. `prepared → archived` directly) are rejected with a 400. This keeps the audit trail honest — `archived` samples must have been `consumed` or `disposed` first.

**See also:**
- Schema: [`migrations/5003_create_samples.sql`](../../migrations/5003_create_samples.sql)
- Parent restriction: [`migrations/5009_samples_parent_restrict.sql`](../../migrations/5009_samples_parent_restrict.sql)
- Routes: [`crates/episcience-api/src/routes/samples.rs`](../../crates/episcience-api/src/routes/samples.rs)

---

## 3.  Protocols

This builds on the kernel concept of *agent* (every protocol has an `authored_by` agent) — see the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

A *protocol* is a versioned, traceable lab SOP — the recipe an experiment instantiates. The `protocols` table stores `title`, an integer `version`, an ordered `steps` JSONB array (each step has an `order`, an `instruction`, and optional `duration_minutes`, `temperature_c`, and `notes`), an `equipment` text array, optional `safety_notes`, a `supersedes` self-reference for the version chain, free-form `labels` + `properties`, and a 32-byte BLAKE3 `content_hash` computed over the steps so clients can detect drift cheaply.

Versioning is enforced by `5008_protocol_version_unique.sql`:

- a unique index on `(supersedes, version)` (partial — `WHERE supersedes IS NOT NULL`) prevents two children of the same parent sharing a version number;
- a check constraint `protocols_root_version_is_one` requires root protocols (`supersedes IS NULL`) to have `version = 1`.

Together: a root protocol starts at v1, each later edit produces a new row pointing to its predecessor with a fresh version number, and the chain is unambiguous. An experiment must cite the specific protocol row used — editing a protocol does not retroactively change what an earlier experiment cited, because the cited row is immutable.

A `POST /api/v1/eln/protocols` request, grounded in `CreateProtocolRequest`:

```json
{
  "title": "Mg2+ buffer exchange for origami",
  "authored_by": "0193a2c1-...-agent-uuid",
  "steps": [
    {"order": 1, "instruction": "Pre-warm TAE-Mg to 25C", "duration_minutes": 10.0, "temperature_c": 25.0},
    {"order": 2, "instruction": "Spin-filter sample at 3000g for 15 min", "duration_minutes": 15.0, "notes": "discard flowthrough"}
  ],
  "equipment": ["centrifuge:eppendorf-5424", "thermomixer"],
  "safety_notes": "EtBr handling: gloves required.",
  "supersedes": "0193a2c0-...-prior-protocol-uuid",
  "labels": ["origami", "buffer-exchange"],
  "properties": {}
}
```

The handler validates the title, hashes the serialized `steps`, and writes a fresh row. The `ProtocolStep` shape is whatever the core type defines — version, title, supersedes, and content_hash are server-managed.

**See also:**
- Schema: [`migrations/5004_create_protocols.sql`](../../migrations/5004_create_protocols.sql)
- Version uniqueness: [`migrations/5008_protocol_version_unique.sql`](../../migrations/5008_protocol_version_unique.sql)
- Routes: [`crates/episcience-api/src/routes/protocols.rs`](../../crates/episcience-api/src/routes/protocols.rs)

---

## 4.  Blobs

This builds on the kernel concept of *claim* — blobs are the raw bytes a claim refers to. See the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

A *blob* is a content-addressed binary attachment — gel image, microscope frame, instrument trace, PDF, raw `.csv`. The `blobs` table stores `filename`, `mime_type`, `size_bytes`, a 32-byte BLAKE3 `content_hash`, the `uploader_id` (an agent), an optional `sample_id` (so blob lineage tracks sample lineage), `labels`, and `properties`. The actual bytes live on the filesystem at `EPISCIENCE_BLOB_DIR/{hash_hex[0:2]}/{hash_hex[2:4]}/{hash_hex}.blob` — a two-level fan-out keeps any one directory's entry count bounded as the dataset grows. The `uploader_id` FK is `ON DELETE RESTRICT`: an agent who has uploaded blobs cannot be hard-deleted without first reassigning or removing them, which is the right default for an ELN.

Because storage is content-addressed by BLAKE3, uploading the same bytes twice produces one file on disk. The `blobs` row that records the upload may be inserted twice (different `filename`, different `properties`, different `sample_id`), but the underlying bytes are deduplicated. This matters for an ELN: the same microscope image cited by three experiments is stored once on disk and indexed three times in metadata — the storage cost is paid for the bytes, not the citations.

Constraints worth knowing:

- `blobs_content_hash_length` — every hash is exactly 32 bytes.
- `blobs_size_positive` — `size_bytes > 0`. Zero-byte blobs are not allowed.
- `blobs_filename_not_empty` — empty filenames are rejected.
- `sample_id` is nullable with `ON DELETE SET NULL`. A blob can outlive its sample, but the link is broken when the sample is removed.

A blob enters the system via `POST /api/v1/eln/blobs` as a multipart upload (fields: `file`, `uploader_id`, optional `sample_id`, `labels`, `properties`). The handler streams the bytes, computes the BLAKE3 hash, writes to disk if the hash isn't already present, and inserts a `blobs` row. A typical row looks like:

```json
{
  "id": "01964c00-...",
  "filename": "tile-batch-12-afm-001.png",
  "mime_type": "image/png",
  "size_bytes": 1843200,
  "content_hash": "b3:7f3a...",
  "uploader_id": "0193a2c1-...",
  "sample_id": "0193a2b8-...",
  "labels": ["afm", "batch-12"],
  "properties": {"instrument": "afm-bruker-3", "scan_size_nm": 500}
}
```

**How claims reference blobs.** A claim does not have a foreign key into `blobs`; the link is by convention. The most common pattern is to embed the blob `id` (or the hex content hash) in the claim's content or properties JSON — e.g. `"AFM scan shows 87% well-formed tiles (see blob 01964c00-...)"`. Where a tighter binding is needed, the synthesis layer can wire a `synthesis_provo_edges` row with `target_kind = 'claim'` and discover the blob through the sample-claim junction. A first-class `claim_blobs` junction table is a likely future addition; until then, the convention plus the `idx_blobs_sample` and `idx_blobs_hash` indices covers the common queries (find all blobs for a sample, find a blob by hash, find duplicate uploads).

Two further consequences of content addressing are worth flagging. First, the `content_hash` is the canonical identifier for the *bytes*; the `id` UUID is the canonical identifier for the *upload record*. Two records can share a hash; one record has exactly one hash. Second, an attacker who tampers with the file on disk cannot escape detection if any verifier rehashes — which is why blob fetch paths should rehash and compare before returning content in any high-stakes flow.

**See also:**
- Schema: [`migrations/5005_create_blobs.sql`](../../migrations/5005_create_blobs.sql)
- Routes: [`crates/episcience-api/src/routes/blobs.rs`](../../crates/episcience-api/src/routes/blobs.rs) (`POST /api/v1/eln/blobs`, `GET /api/v1/eln/blobs/:id/download`)

---

## 5.  Countersignatures

This builds on the kernel concept of *signature* on a claim — see the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

A *countersignature* is a second-or-later agent's Ed25519 signature on a claim that has already been signed by its author. It encodes peer-review-style attestation: "I, agent X, witnessed / approved / reviewed / certified / countersigned this claim." The five meanings are not synonyms — they encode different levels of commitment. `witnessed` says "I saw this happen"; `approved` says "I bless this for downstream use"; `reviewed` says "I checked the work"; `certified` says "I take regulatory or contractual responsibility"; `countersigned` is the generic catchall. A lab's policy decides which meaning gates which workflow.

The `countersignatures` table pins `claim_id`, `signer_id`, a `signature_meaning` constrained to the five values above, a 32-byte BLAKE3 `content_hash`, the 64-byte `signature`, and (since `5010`) a `prev_signature_hash` for hash-chaining and a `signature_version` smallint.

The chain is the interesting part. Migration `5010_countersign_chain.sql` adds:

- `prev_signature_hash BYTEA` — the BLAKE3 hash of the immediately prior countersignature on the same claim, or `NULL` for the first one. Constrained to exactly 32 bytes when non-null.
- `signature_version SMALLINT NOT NULL DEFAULT 1` — lets us evolve the canonical message format without breaking historical verification. The route handler currently writes version 2, whose canonical message is `claim_id|signer_id|signature_meaning|content`. Verification falls back to the version-1 format (raw content) for older rows.

So the per-claim sequence of countersignatures forms a tamper-evident chain: tampering with any row breaks the `prev_signature_hash` link of the row that follows. Combined with `cs_unique_signer_claim (claim_id, signer_id, signature_meaning)`, this prevents the same agent from countersigning the same claim with the same meaning twice — useful when the lab policy is "two distinct approvers, both with `approved` meaning."

A `POST /api/v1/eln/countersign` request, grounded in `CountersignRequest`:

```json
{
  "claim_id": "01964b00-...-claim-uuid",
  "signer_id": "0193a2c1-...-signer-uuid",
  "signature_meaning": "approved",
  "signature_hex": "<128 hex chars: 64-byte Ed25519 sig over the canonical message>",
  "public_key_hex": "<64 hex chars: 32-byte public key>"
}
```

The handler validates the meaning string, fetches the claim content, recomputes the canonical message `claim_id|signer_id|signature_meaning|content`, verifies the Ed25519 signature against the supplied public key, and only then inserts the row. The `auth.agent_id == signer_id` check is enforced server-side, so an agent cannot countersign on behalf of another.

`GET /api/v1/eln/claims/:claim_id/countersignatures/verify` re-derives and rechecks every row on a claim — re-verification is cheap and is the right call before relying on an approval. The verifier returns per-row `content_hash_valid` and `signature_valid` booleans so a downstream UI can highlight any countersignature whose hash no longer matches (claim was edited after signing) or whose signature no longer verifies (public key rotated or compromised). For version-1 rows the canonical message is the raw claim content; for version-2 rows it is the four-field pipe-delimited string above. New writes always use version 2; older rows verify against version 1 so historical attestations remain checkable.

One last subtlety: because the canonical message includes the claim *content*, editing a signed claim breaks every countersignature on it. That is the point — an attestation is to a specific content snapshot, not a mutable handle. If a claim is corrected post-signing, the corrected version must collect fresh countersignatures.

**See also:**
- Schema: [`migrations/5006_create_countersignatures.sql`](../../migrations/5006_create_countersignatures.sql)
- Chain + versioning: [`migrations/5010_countersign_chain.sql`](../../migrations/5010_countersign_chain.sql)
- Routes: [`crates/episcience-api/src/routes/countersign.rs`](../../crates/episcience-api/src/routes/countersign.rs)

---

## 6.  Synthesis claims and PROV-O edges

This builds on the kernel concept of *edge* — synthesis edges sit alongside kernel epistemic edges but in a separate table. See the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

A *synthesis claim* is a higher-level narrative generated by clustering a subgraph and asking an LLM to summarize it. The `syntheses` table pins the originating `query`, the `agent_id`, a `status` lifecycle (`pending → running → complete | failed | deleted`), an optional `parent_synthesis_id` for refinement chains, the generated `narrative` (with `narrative_format = 'markdown'`), the captured `subgraph_snapshot` JSONB, `clustering_method` (currently constrained to `signed_louvain`), `llm_provider`/`llm_model`, optional `prereq_synthesis_ids`, timestamps, a `content_hash`, a `visibility` (`private | shared | public`), and a `stale_since`/`stale_reason` pair (the reason is constrained to `belief_drift | new_contradiction | claim_superseded | frame_changed | edge_revoked`). The table's check constraints enforce internal consistency — for example, `(status = 'complete') = (narrative IS NOT NULL)` and `(stale_since IS NULL) = (stale_reason IS NULL)` — so a row can never be half-completed or half-stale at the database level.

A `POST /api/v1/eln/syntheses` request kicks off the worker:

```json
{
  "query": "What does batch 12 say about tile yield?",
  "parent_synthesis_id": null,
  "prereq_synthesis_ids": [],
  "visibility": "private"
}
```

The handler atomically inserts a `syntheses` row in `pending` status and a `synthesis_jobs` row in `queued` state, then returns `202 Accepted` with the new id. The worker picks up the job, drives the row through clustering, LLM narration, edge materialization, and completes it. Refinement is `POST /api/v1/eln/syntheses/:id/refine`, which creates a *new* synthesis row pointing to the parent — the parent is never mutated, so refinement chains form a tree.

Synthesis claims connect back into the kernel through two tables:

- **`synthesis_claim_membership`** — which kernel claims went into the cluster the synthesis narrates. Pure membership, no semantics beyond "this claim contributed."
- **`synthesis_provo_edges`** — provenance-style dependency edges, with a `predicate` constrained to exactly four values:

  | Predicate         | Origin                | Meaning in episcience                                  |
  | ----------------- | --------------------- | ------------------------------------------------------ |
  | `WAS_DERIVED_FROM`| W3C PROV-O            | This synthesis was derived from the target row.        |
  | `REFINES`         | episcience-specific   | This synthesis refines (replaces, sharpens) the target.|
  | `COMPOSED_OF`     | episcience-specific   | This synthesis is composed of the listed components.   |
  | `ATTRIBUTED_TO`   | episcience-specific (PROV-O-inspired) | This synthesis is attributed to the target agent. |

  Only `WAS_DERIVED_FROM` is from the W3C PROV-O standard set; the other three are episcience-specific predicates. The `target_kind` column is restricted to `claim | synthesis | agent`, so the edge can point at a kernel claim, another synthesis, or the responsible agent. An `epigraph_edge_id` column lets episcience pair each PROV-O edge with the kernel edge it mirrors once the worker has written it out — see `synthesis_provo_edges_pending_idx` for the queue of rows still awaiting that write.

**Why separate from kernel epistemic edges?** Episcience splits two ideas the EpiGraph kernel currently conflates via the overloaded `supports` edge: *dependency provenance* (what was derived from what) and *epistemic stance* (what supports, refutes, or refines what). A PROV-O `WAS_DERIVED_FROM` edge from a synthesis to a claim does not mean the synthesis *supports* that claim — it means the synthesis was *generated by reading it*. Mixing the two means a downstream BetP computation cannot tell whether an edge expresses belief or merely lineage. Keeping the two tables apart preserves the kernel's epistemic semantics and gives the science layer a clean place to record dependency without polluting belief propagation.

**See also:**
- Synthesis schema: [`migrations/synthesis/5011_create_syntheses.sql`](../../migrations/synthesis/5011_create_syntheses.sql)
- Membership: [`migrations/synthesis/5017_create_synthesis_claim_membership.sql`](../../migrations/synthesis/5017_create_synthesis_claim_membership.sql)
- PROV-O edges: [`migrations/synthesis/5018_create_synthesis_provo_edges.sql`](../../migrations/synthesis/5018_create_synthesis_provo_edges.sql)
- Routes: [`crates/episcience-api/src/routes/syntheses.rs`](../../crates/episcience-api/src/routes/syntheses.rs)

---

## 7.  Synthesis skills

This builds on the kernel concept of *claim* — a skill is the policy object that shapes how a synthesis claim is composed and verified. See the [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) for the kernel pattern.

A *synthesis skill* is a pluggable specialisation that contributes per-stage prompt sections, an optional traversal-config default, and an optional verification rubric to the synthesis pipeline. The pattern is borrowed from SciLink's foundation-agent design: a closed *section vocabulary* defines the stages a pipeline knows about, and any number of skills can be plugged in to provide stage-specific content. Skills are Rust types behind the `SynthesisSkill` trait (in `crates/episcience-core/src/synthesis/skill.rs`); the active skill for a given synthesis is named by `syntheses.skill_name` and resolved at job-pickup time. There is no impersonation: a skill name that doesn't match a registered skill falls back to `baseline` with a logged warning rather than blocking the worker.

The section vocabulary is the `SynthesisStage` enum, an **eight-variant closed set**: `Overview`, `Planning`, `Traversal`, `Clustering`, `Narration`, `Composition`, `Verification`, `Novelty`. Adding a new stage is a deliberate pipeline change — skills cannot invent stages, they can only contribute to existing ones. The naming mirrors SciLink's `overview / planning / implementation / interpretation / validation` set, extended with the stages specific to graph-clustering synthesis (`Traversal`, `Clustering`, `Composition`, `Novelty`).

The `SynthesisSkill` trait surface is intentionally small:

- `name(&self) -> &'static str` — stable identifier persisted in `syntheses.skill_name`. Lowercase snake_case; must match the registry key.
- `section(&self, stage: SynthesisStage) -> Option<&str>` — skill-specific prompt section for `stage`, or `None` to fall back to the pipeline's baseline prompt.
- `traversal_config(&self) -> Option<TraversalConfig>` — default-method returns `None`. Skills with strong domain opinions override.
- `verify(&self, ctx: &VerificationContext) -> VerificationOutcome` — default-method delegates to `default_citation_rubric`. Skills with stricter checks override.

Two skills are registered today:

- **`BaselineSkill`** (in `crates/episcience-core/src/synthesis/skills/baseline.rs`) is the default. It encodes the pre-skill pipeline's inline prompts for `Overview`, `Narration`, `Composition`, and `Verification`. It is intentionally behaviour-preserving — a synthesis written before the skill refactor and one written today through `baseline` produce byte-identical prompts.
- **`LabNotebookSkill`** (in `crates/episcience-core/src/synthesis/skills/lab_notebook.rs`) is ELN-tuned. It overrides `Narration` (asks for chronological 2–4-sentence summaries that mention protocols and samples by id), `Composition` (compose the per-cluster summaries oldest-first), and `traversal_config` (max-hops 2, edge types narrowed to `Supports` + `Corroborates`, `relevance_prune` 0.55). The narrower edge-type set deliberately excludes `Contradicts`, `Supersedes`, and `Methodology` — they widen into argumentative or methodological lineage the chronological narrative doesn't want.

Selection is a single field on the create request:

```json
{
  "query": "What happened in batch 12?",
  "skill_name": "lab_notebook",
  "visibility": "private"
}
```

Omitting `skill_name` defaults to `"baseline"` (the default is server-side, written into the row's `skill_name` column at insert time, so the row always carries an explicit value). Refinement inherits the parent's `skill_name`: `POST /api/v1/eln/syntheses/:id/refine` reads the parent's `skill_name` and re-uses it for the child, so a refinement chain runs the same skill end-to-end unless a caller explicitly varies it.

The registry is closed-set at compile time. Adding a new skill is two file edits and a one-line migration:

1. New `crates/episcience-core/src/synthesis/skills/<name>.rs` with the `impl SynthesisSkill`.
2. Register the variant in `crates/episcience-core/src/synthesis/skills/mod.rs::load_by_name`.
3. Migration that extends the `syntheses_skill_name_known` CHECK constraint to include the new name.

The CHECK constraint is co-evolved with the registry on purpose: a typo in a write path that produces an unknown `skill_name` fails at insert rather than silently falling back. Reads tolerate unknown names (with a warning) because a stale row should never block the worker, but writes are strict.

**See also:**
- Trait + section vocabulary: [`crates/episcience-core/src/synthesis/skill.rs`](../../crates/episcience-core/src/synthesis/skill.rs)
- Registry: [`crates/episcience-core/src/synthesis/skills/mod.rs`](../../crates/episcience-core/src/synthesis/skills/mod.rs)
- BaselineSkill: [`crates/episcience-core/src/synthesis/skills/baseline.rs`](../../crates/episcience-core/src/synthesis/skills/baseline.rs)
- LabNotebookSkill: [`crates/episcience-core/src/synthesis/skills/lab_notebook.rs`](../../crates/episcience-core/src/synthesis/skills/lab_notebook.rs)
- Column + CHECK constraint: [`migrations/synthesis/5020_syntheses_skill_column.sql`](../../migrations/synthesis/5020_syntheses_skill_column.sql)
- CHECK extension for second skill: [`migrations/synthesis/5022_syntheses_skill_lab_notebook.sql`](../../migrations/synthesis/5022_syntheses_skill_lab_notebook.sql)
- Route handler: [`crates/episcience-api/src/routes/syntheses.rs`](../../crates/episcience-api/src/routes/syntheses.rs) (`enqueue_synthesis`, `refine_synthesis`)
- Glossary entries: [skill (synthesis)](04-glossary.md#skill-synthesis), [skill registry](04-glossary.md#skill-registry), [skill section](04-glossary.md#skill-section), [synthesis stage](04-glossary.md#synthesis-stage), [baseline skill](04-glossary.md#baseline-skill), [lab_notebook skill](04-glossary.md#lab_notebook-skill)

---

## 8.  Verifier-driven acceptance

This builds on §7 (synthesis skills) and §6 (synthesis claims). Verification is Stage 6 of the synthesis pipeline — it runs after composition, before the row is allowed to move to `status = 'complete'`.

A *verifier* is a per-skill rubric that gates acceptance of a generated narrative. Stage 5 produces a Markdown narrative composed of per-cluster summaries; Stage 6 runs the active skill's `verify(...)` method against the narrative plus the cluster context and returns a `VerificationOutcome`. Only `Accept` moves the row to `complete`; `Reject` either spawns a refinement child (§10) or terminates the row in `rejected`.

`VerificationOutcome` is a serde-tagged enum with two arms:

| Arm | Fields | Meaning |
| --- | --- | --- |
| `Accept` | `rubric`, `evidence` | The narrative passed the rubric. `evidence` is free-form structured JSON (e.g. `{"cited_count": 7}`). |
| `Reject` | `rubric`, `reason`, `evidence` | The narrative failed the rubric. `reason` is one of four reason variants below. |

`VerificationReason` carries the four reject reasons:

- `UncitedMember { claim_id }` — a claim in the cluster was not cited anywhere in the narrative.
- `HallucinatedCitation { claim_id }` — a citation referred to a claim outside the cluster.
- `KernelContradiction { claim_id }` — reserved for future use; the default rubric does not check this.
- `SkillRejection { detail }` — skill-specific veto with free-form detail.

The default rubric (`default_citation_rubric` in `crates/episcience-core/src/synthesis/verifier.rs`) is the strict citation rubric `BaselineSkill::verify` delegates to: every cluster member must appear in at least one `[<claim_id>]` citation, and no citation may refer to a claim outside the cluster. Skills override `verify` to add stricter checks (e.g. mandatory protocol cites for lab-notebook); they cannot weaken the default below the citation rule without explicit opt-in via the refinement temperature (§10).

The status lifecycle gains two new states with this stage:

| Status | Meaning |
| --- | --- |
| `pending` | Inserted, waiting for the worker. |
| `running` | Worker is in stages 1–5 (seed → traverse → cluster → narrate → compose). |
| `verifying` | Worker has finished composition and is running Stage 6. |
| `complete` | Verifier returned `Accept`; narrative published, edges materialised, novelty (§9) scored. |
| `failed` | Worker crashed before completing (LLM unavailable, transient DB error, etc.). |
| `rejected` | Verifier returned `Reject` and either the refinement ceiling is hit or refinement is disabled. |
| `deleted` | Soft-deleted (owner-only via `DELETE /syntheses/:id`). |

Persistence: the `syntheses` row gains two columns the verifier writes on every run, whether it accepts or rejects:

- `verifier_outcome JSONB` — the full `VerificationOutcome` payload, serde-tagged. Example accept value: `{"kind":"accept","rubric":"default_citation","evidence":{"cited_count":2}}`. Example reject value: `{"kind":"reject","rubric":"default_citation","reason":{"uncited_member":{"claim_id":"0193a2c1-..."}},"evidence":{"cited":["0193a2c0-..."]}}`.
- `verifier_attempts SMALLINT NOT NULL DEFAULT 0` — incremented on every verifier run. A refinement child starts at 0 and runs the verifier fresh; the count is per-row, not per-chain.

```json
{
  "id": "01964c00-...",
  "status": "complete",
  "verifier_outcome": {
    "kind": "accept",
    "rubric": "default_citation",
    "evidence": {"cited_count": 2}
  },
  "verifier_attempts": 1
}
```

**See also:**
- Verifier types + default rubric: [`crates/episcience-core/src/synthesis/verifier.rs`](../../crates/episcience-core/src/synthesis/verifier.rs)
- `SynthesisSkill::verify` default method: [`crates/episcience-core/src/synthesis/skill.rs`](../../crates/episcience-core/src/synthesis/skill.rs)
- Status + verifier columns: [`migrations/synthesis/5021_syntheses_verifier_outcome.sql`](../../migrations/synthesis/5021_syntheses_verifier_outcome.sql)
- Glossary entries: [verification outcome](04-glossary.md#verification-outcome), [verification rubric](04-glossary.md#verification-rubric)

---

## 9.  Novelty assessment

This builds on §8 (verifier-driven acceptance) — novelty is Stage 7, which runs only when Stage 6 accepts. It is the final stage in the synthesis pipeline.

A *novelty score* answers "how much new ground does this synthesis cover?" against the existing set of completed syntheses (or, with a pluggable backend, against external literature). It is computed once, persisted on the row, and never recomputed for that row — a synthesis's novelty is a snapshot of the graph at acceptance time, not a moving target.

`NoveltyScore` carries:

- `score: f64` — 0.0 (fully redundant) to 1.0 (highly novel). Computed.
- `backend: String` — the backend that produced the score (matches `NoveltyBackend::name`).
- `neighbours: Vec<NoveltyNeighbour>` — top-5 prior syntheses ranked by similarity, descending.
- `rationale: String` — free-form rationale text from the backend.

Each `NoveltyNeighbour` is `{ synthesis_id, similarity, member_overlap }` — the aggregate similarity and the raw Jaccard overlap of cluster members between the candidate and the prior.

The default backend (`InternalNoveltyBackend` in `crates/episcience-db/src/synthesis/novelty_backend_internal.rs`, name `"internal_prior_syntheses"`) scores against prior `complete` syntheses sharing at least one cluster member. The algorithm:

1. Embed the candidate narrative once.
2. For each prior synthesis that overlaps the candidate's cluster, compute `similarity = 0.5 * cosine(narrative_emb) + 0.5 * jaccard(member_ids)`.
3. Keep the top 5 by aggregate similarity.
4. `score = (1.0 - top_similarity).clamp(0.0, 1.0)`.

When no prior synthesis shares any cluster member, the score is 1.0 and `neighbours` is empty — there is nothing to be redundant against. This is the common case on a fresh kernel and is the right answer (not a flag to special-case).

The `NoveltyBackend` trait (in `crates/episcience-core/src/synthesis/novelty.rs`) is pluggable: a future backend can score against PubMed, arXiv, or any other external corpus. The trait surface is small — `name()` plus an async `score(candidate_synthesis_id, candidate_narrative, candidate_member_ids) -> Result<NoveltyScore, NoveltyError>` — so a new backend is one file + one config knob.

**Failures are non-fatal.** If the backend returns `NoveltyError::Unavailable` (e.g. the embedder is down, or an external API rate-limited), the worker logs but does not fail the synthesis. The row moves to `complete` without `novelty_score`; a later batch job can backfill. This is the deliberate trade-off: novelty is *useful information*, not a load-bearing gate, so a degraded scoring path must not punish the underlying narrative.

Persistence: two columns on the `syntheses` row.

- `novelty_score JSONB` — the full `NoveltyScore` payload. Example: `{"score":0.83,"backend":"internal_prior_syntheses","neighbours":[{"synthesis_id":"01964b00-...","similarity":0.17,"member_overlap":0.33}],"rationale":"top-prior similarity 0.167"}`.
- `novelty_backend TEXT` — the backend name, denormalised for cheap `WHERE` filters.

**See also:**
- Types + trait: [`crates/episcience-core/src/synthesis/novelty.rs`](../../crates/episcience-core/src/synthesis/novelty.rs)
- Default backend: [`crates/episcience-db/src/synthesis/novelty_backend_internal.rs`](../../crates/episcience-db/src/synthesis/novelty_backend_internal.rs)
- Schema: [`migrations/synthesis/5023_syntheses_novelty.sql`](../../migrations/synthesis/5023_syntheses_novelty.sql)
- Glossary entries: [novelty score](04-glossary.md#novelty-score), [novelty backend](04-glossary.md#novelty-backend)

---

## 10.  Refinement chains

This builds on §6 (synthesis claims and the `REFINES` PROV-O edge), §7 (skills), and §8 (verifier reject path). Refinement is what happens when Stage 6 rejects.

A *refinement chain* is a parent → child sequence of synthesis rows where each child is a fresh attempt at the same query with progressively widened search parameters. The pattern is borrowed from SciLink's "simulated-annealing agentic pipelines": hold priors strict at first, then thaw progressively as iterations fail. When the verifier rejects, the worker does not just mark the row `rejected`; it spawns a child synthesis with a thawed `RefinementTemperature` and re-runs the pipeline from Stage 1.

`RefinementTemperature` (in `crates/episcience-core/src/synthesis/refinement.rs`) carries three knobs:

- `depth_delta: u8` — hops added on top of the parent's traversal config. Bounded at 3.
- `relevance_prune_relax: f32` — multiplier on `relevance_prune` (smaller → keeps more neighbours). Bounded at 0.4 floor (below ~0.22 effective threshold becomes noise).
- `allow_soft_verifier: bool` — true after the first reject. The default rubric does not honor this knob today; future skill rubrics may downgrade strict checks (e.g. "every member must be cited" → "≥50% of members"). It is a forward-compatible signal, not a current behaviour.

The default value is "cold": `{ depth_delta: 0, relevance_prune_relax: 1.0, allow_soft_verifier: false }`. `anneal()` advances one step (`depth_delta + 1` capped at 3, `relevance_prune_relax * 0.8` floored at 0.4, `allow_soft_verifier = true`). `at_ceiling()` returns true when `depth_delta >= 3`; the worker uses this to stop spawning further children.

Refinement flow on reject:

1. Worker reads the rejected row's `refinement_temperature`.
2. If `at_ceiling()`, the row stays in `rejected` and no child is created — refinement has converged on failure.
3. Otherwise, `temp = temp.anneal()`. A new `syntheses` row is inserted in `pending`, inheriting the parent's `skill_name` and carrying the annealed temperature. A `synthesis_provo_edges` row of predicate `REFINES` is written from child to parent.
4. A fresh job is enqueued for the child. The parent stays in `rejected` — it is not retroactively repaired; the child is the new attempt.

A refinement chain is therefore a sequence of **sibling rows** in the `syntheses` table, joined by `REFINES` edges, where exactly one leaf is the live attempt and all interior rows are terminal rejections. The maximum chain length is 4 (the root attempt plus three refinements at `depth_delta` 1, 2, 3); beyond that the leaf is a terminal `rejected` row.

Inspecting a chain:

```sql
SELECT s.id, s.status, s.verifier_attempts,
       (s.refinement_temperature->>'depth_delta')::int AS depth_delta,
       p.target_id AS parent_id
  FROM syntheses s
  LEFT JOIN synthesis_provo_edges p
    ON p.synthesis_id = s.id
   AND p.predicate = 'REFINES'
   AND p.target_kind = 'synthesis'
 WHERE s.id = ANY($1::uuid[])
 ORDER BY (s.refinement_temperature->>'depth_delta')::int;
```

(`synthesis_provo_edges` keys the row by `synthesis_id` — the child synthesis owning the edge — pointing at `target_id` of `target_kind = 'synthesis'`: the parent it refines. See [`migrations/synthesis/5018_create_synthesis_provo_edges.sql`](../../migrations/synthesis/5018_create_synthesis_provo_edges.sql) for the column shape.)

Persistence: one new column.

- `refinement_temperature JSONB` — the temperature carried by the row. Default JSON `{"depth_delta":0,"relevance_prune_relax":1.0,"allow_soft_verifier":false}`. A child's temperature is the parent's `anneal()`d value.

**See also:**
- Type + anneal: [`crates/episcience-core/src/synthesis/refinement.rs`](../../crates/episcience-core/src/synthesis/refinement.rs)
- Schema: [`migrations/synthesis/5024_syntheses_refinement_temperature.sql`](../../migrations/synthesis/5024_syntheses_refinement_temperature.sql)
- PROV-O `REFINES` edge schema: [`migrations/synthesis/5018_create_synthesis_provo_edges.sql`](../../migrations/synthesis/5018_create_synthesis_provo_edges.sql)
- Job-handler reject path: [`crates/episcience-api/src/jobs/synthesis_job.rs`](../../crates/episcience-api/src/jobs/synthesis_job.rs)
- Glossary entries: [refinement chain](04-glossary.md#refinement-chain), [refinement temperature](04-glossary.md#refinement-temperature), [REFINES edge](04-glossary.md#refines-edge)

---

## 11.  Protocol section vocabulary

This builds on §3 (protocols) — sections are an **additive** structured-content companion to the existing `steps` + `equipment` + `safety_notes` shape. Existing protocols continue to work unchanged; the new `sections` column defaults to `'{}'::jsonb` and is opt-in on writes.

A *protocol section* is a named string of guidance an agent reads at a known decision point of the protocol's lifecycle. The five named sections mirror SciLink's foundation-agent set: `overview`, `planning`, `implementation`, `interpretation`, `validation`. Each is optional. Together they describe *what the protocol is for*, *how to plan a run of it*, *how to execute*, *how to interpret the output*, and *how to validate the output is trustworthy*. The original `steps` array remains the canonical "what to do, in order"; the section vocabulary is the surrounding judgment-and-context layer.

`ProtocolSections` (in `crates/episcience-core/src/protocol.rs`) is the struct:

```rust
pub struct ProtocolSections {
    pub overview: Option<String>,
    pub planning: Option<String>,
    pub implementation: Option<String>,
    pub interpretation: Option<String>,
    pub validation: Option<String>,
    pub extras: HashMap<String, String>,
}
```

On `POST /api/v1/eln/protocols`, the request body may include a `sections` field. Off-vocabulary keys — anything not in the five named — are **preserved**, not rejected. They land under `sections.extras` (a free-form map) and the response carries an `X-Episcience-Protocol-Warnings` header listing the dropped-into-extras keys so the client can correct its terminology without losing data. The behaviour mirrors SciLink's loader-warning pattern: be permissive on writes, but visible about what was non-canonical.

A `POST /api/v1/eln/protocols` request with sections:

```json
{
  "title": "Mg2+ buffer exchange for origami",
  "authored_by": "0193a2c1-...-agent-uuid",
  "steps": [
    {"order": 1, "instruction": "Pre-warm TAE-Mg to 25C"},
    {"order": 2, "instruction": "Spin-filter at 3000g for 15 min"}
  ],
  "equipment": ["centrifuge:eppendorf-5424"],
  "sections": {
    "overview": "Exchange Tris/EDTA buffer into TAE-Mg for AFM imaging.",
    "planning": "Allocate 30 minutes; ensure thermomixer is pre-warmed.",
    "implementation": "Follow steps in order; record any deviations under properties.deviations.",
    "interpretation": "Recovery > 70% indicates a clean exchange.",
    "validation": "Run an OD260 before/after to confirm.",
    "tips": "Pre-cool centrifuge to 4C if working with thermally fragile constructs."
  }
}
```

The response stores `overview`/`planning`/`implementation`/`interpretation`/`validation` in their named slots and `"tips"` under `sections.extras`. The response carries:

```
X-Episcience-Protocol-Warnings: extras_dropped=tips
```

The `ProtocolSections::from_value` helper (also in `protocol.rs`) is the canonical parser — it routes known keys to their named slots, off-vocab keys to `extras`, and returns the list of off-vocab keys observed so the handler can build the warning header. Non-string values for known keys are dropped silently (the parser keeps shape forgiving on writes).

**See also:**
- Type + parser: [`crates/episcience-core/src/protocol.rs`](../../crates/episcience-core/src/protocol.rs) (`ProtocolSections`, `ProtocolSections::from_value`)
- Schema (additive `sections` column): [`migrations/5025_protocols_section_vocabulary.sql`](../../migrations/5025_protocols_section_vocabulary.sql)
- Route handler: [`crates/episcience-api/src/routes/protocols.rs`](../../crates/episcience-api/src/routes/protocols.rs)
- Glossary entries: [skill section](04-glossary.md#skill-section) (parallel concept on the synthesis side)

---

## 12.  Workflow runs

This builds on §2 (samples) — a *workflow run* is a sample whose `sample_type` is the literal string `workflow_run`, recording the fact that an out-of-band agent (an EpiClaw scheduled task, today; potentially any orchestrator tomorrow) executed a named EpiGraph workflow at a given instant. Downstream observations, blobs, and countersignatures attach to it exactly the way they would to a biological or material sample — the loop's plumbing does not need to know that the "material" being observed is a process run rather than a vial.

The shape is deliberately thin. A `workflow_run` sample's `name` is the workflow's `canonical_name`; the EpiGraph workflow UUID is carried in `properties.workflow_id`; `preparation_date` is the run's `started_at`; the `workflow_run` literal also appears in `labels` so a label-only query finds every run without a JSONB scan. The `samples_sample_type_check` CHECK constraint (added by migration `5026_samples_workflow_run.sql`) is the canonical list of permitted `sample_type` values — `'biological' | 'chemical' | 'material' | 'composite' | 'workflow_run'`. Adding a future sample-type variant requires editing both the `SampleType` enum and the CHECK constraint, the same co-evolution rule the synthesis-skill registry uses (§7).

`POST /api/v1/eln/workflow_runs` is the dedicated write path, grounded in `CreateWorkflowRunRequest`:

```json
{
  "workflow_id": "0193a2c0-...-workflow-uuid",
  "canonical_name": "research-scan-morning",
  "prepared_by": "0193a2c1-...-agent-uuid",
  "started_at": "2026-05-28T09:00:00Z",
  "labels": ["arxiv", "morning-batch"]
}
```

The handler enforces `auth.agent_id == prepared_by` (no third-party run reporting), bypasses the generic `SampleRepository::create` path (which stamps `preparation_date = NOW()` unconditionally) so the caller's `started_at` is honoured verbatim, computes a BLAKE3 `content_hash` over `canonical_name || workflow_id_bytes || started_at_rfc3339`, appends `"workflow_run"` to the supplied labels, and inserts a row in `status = 'prepared'`. The response carries the new `sample_id`, the canonicalised `sample_type`, and the original `workflow_id` for round-trip convenience. From here the run is just another sample: observation claims attach via §1's `POST /api/v1/eln/samples/:id/observations`, blobs via §4's `POST /api/v1/eln/blobs`, and a synthesis (typically the per-workflow skill of §13) can be enqueued naming the run in its query.

The point of giving workflow runs first-class sample identity is that *every other ELN affordance still works against them*. A countersignature on a `workflow_run` sample's observation claim attests "I, agent X, agree this run produced what it claims to have produced," and the same chain rules (§5) apply. A frame can be drawn around the runs of one campaign and BetP propagated normally. A synthesis can cluster across runs and the existing `sample_claims` junction is enough to ground the claims. Nothing in the science layer needs to know that the upstream cause was a cron schedule rather than a lab technician at a bench.

**See also:**
- Route handler: [`crates/episcience-api/src/routes/workflow_runs.rs`](../../crates/episcience-api/src/routes/workflow_runs.rs) (`create_workflow_run`, `POST /api/v1/eln/workflow_runs`)
- SampleType variant: [`crates/episcience-core/src/sample.rs`](../../crates/episcience-core/src/sample.rs) (`SampleType::WorkflowRun`)
- Schema CHECK constraint: [`migrations/5026_samples_workflow_run.sql`](../../migrations/5026_samples_workflow_run.sql)
- EpiClaw caller-side hook (`workflow_run` creation, observation attachment): [epiclaw-host PR #15](https://github.com/tylorsama/epiclaw-host/pull/15), [PR #16](https://github.com/tylorsama/epiclaw-host/pull/16)
- Glossary entries: [workflow_run sample](04-glossary.md#workflow_run-sample), [WorkflowRunHook](04-glossary.md#workflowrunhook-epiclaw)

---

## 13.  Per-workflow synthesis skills

This builds on §7 (synthesis skills) and §12 (workflow runs). The skill foundation laid down for `baseline` + `lab_notebook` is extended here with three skills tuned for the three classes of EpiClaw scheduled task that have crystallised in practice: literature scanning, code review, and capability-registry diffing. The shape is unchanged — each new skill is a `SynthesisSkill` impl with its own `name`, `section(stage)` overrides, optional `traversal_config`, and (in one case) a custom `verify`. Each ships its own per-name CHECK migration co-evolving the `syntheses_skill_name_known` constraint.

The end-to-end trigger flow is uniform across all three. An EpiClaw scheduled task in `schedules.toml` carries a `synthesis_skill = "<name>"` field. When the task fires and completes successfully, EpiClaw's `WorkflowRunHook` (see EpiClaw's `docs/integration-with-episcience.md`) posts a `workflow_run` sample (§12), records any task output as a measurement claim, and then `POST /api/v1/eln/syntheses` with `skill_name = "<name>"`. The synthesis worker picks the row up and runs the seven-stage pipeline with the named skill's section overrides and traversal config in place. None of the skills below override Stage 6 except `code_review`; the others inherit the default citation rubric from §8.

### 13.1  LiteratureSkill (`"literature"`)

Tuned for the arxiv research-scan workflow. `Narration` demands DOI/arxiv citation formatting (`[<claim_id>] (doi:10.xxx/yyy)` or `(arxiv:NNNN.NNNNN)` when no DOI exists); `Composition` orders the per-cluster summaries by methodology family then publication date; `traversal_config` widens to `max_hops = 3` over the citation-discipline trio of edge types — `Supports`, `Methodology`, `Corroborates`. The three-hop reach is the widest of any shipped skill: literature scans are explicitly looking for unfamiliar citation chains, so the cost of pulling in tangential nodes is worth the recall. Verification falls back to the default citation rubric — the literature-specific quality signal is novelty, not citation discipline, and that lives in §14.

Files: [`crates/episcience-core/src/synthesis/skills/literature.rs`](../../crates/episcience-core/src/synthesis/skills/literature.rs), prose bundle [`crates/episcience-core/src/synthesis/skills/markdown/literature.md`](../../crates/episcience-core/src/synthesis/skills/markdown/literature.md), CHECK extension [`migrations/synthesis/5028_syntheses_skill_literature.sql`](../../migrations/synthesis/5028_syntheses_skill_literature.sql).

### 13.2  CodeReviewSkill (`"code_review"`)

Tuned for the nightly-bug-fix pipeline. `Narration` asks for PR-body-shaped 3-5 sentence summaries with `[<claim_id>]` citations and `#<number>` for PRs, `` `<sha>` `` (7-char abbreviation acceptable) for commits; `Composition` produces a Markdown narrative organised as `## Summary` / `## Files changed` / `## Test plan` — the standard PR shape — so the synthesis row's narrative is *itself* a draft PR body. `traversal_config` narrows to `max_hops = 2` over `Supports + Methodology` with `relevance_prune = 0.6`.

The verifier override is the strict part. After running the default citation rubric (and bailing on any baseline reject), `CodeReviewSkill::verify` adds a *PR-citation proximity* check: every `#NNNN` mentioned in the narrative must appear within 120 characters (on either side) of a `[<claim_id>]` citation. A PR number floating without a nearby claim citation is a `SkillRejection { detail: "PR #NNNN mentioned without a nearby [<claim_id>] citation" }` with rubric `"code_review_pr_citation"`. The strictness is appropriate here because the narrative may become a merge gate (Workflow E in [`05-workflows.md`](05-workflows.md#workflow-e--countersign-as-merge-gate-review-bot)) — a hallucinated PR reference must not slip past Stage 6. The deeper "does the cited claim actually carry a `pr_number` property" check belongs at the review-bot tier; the verifier sees only the narrative and the member ids.

Files: [`crates/episcience-core/src/synthesis/skills/code_review.rs`](../../crates/episcience-core/src/synthesis/skills/code_review.rs) (`CodeReviewSkill::verify`), prose bundle [`crates/episcience-core/src/synthesis/skills/markdown/code_review.md`](../../crates/episcience-core/src/synthesis/skills/markdown/code_review.md), CHECK extension [`migrations/synthesis/5029_syntheses_skill_code_review.sql`](../../migrations/synthesis/5029_syntheses_skill_code_review.sql).

### 13.3  RegistryDiffSkill (`"registry_diff"`)

Tuned for the weekly-capability-audit workflow — what tools were added, removed, or drifted in schema since the last audit. `Narration` asks for per-cluster lists of capability changes marked with `+` (added), `-` (removed), `~` (drifted), each citing `[<claim_id>]`; `Composition` produces three Markdown tables — `## Added` / `## Removed` / `## Drifted` — with columns Tool / Version / Notes / `[<claim_id>]`. `traversal_config` is the shallowest of the shipped skills: `max_hops = 1` over `Supersedes` only. The narrowness is the point — tool versions chain through `Supersedes`, so a one-hop traversal at the registry tier reaches "the previous version" without diluting with general supports/methodology lineage. Verification falls back to the default citation rubric; the "every Removed row should carry an `epigraph_edge_id`" check is review-bot tier, not verifier.

Files: [`crates/episcience-core/src/synthesis/skills/registry_diff.rs`](../../crates/episcience-core/src/synthesis/skills/registry_diff.rs), prose bundle [`crates/episcience-core/src/synthesis/skills/markdown/registry_diff.md`](../../crates/episcience-core/src/synthesis/skills/markdown/registry_diff.md), CHECK extension [`migrations/synthesis/5030_syntheses_skill_registry_diff.sql`](../../migrations/synthesis/5030_syntheses_skill_registry_diff.sql).

**See also:**
- Skill trait: [`crates/episcience-core/src/synthesis/skill.rs`](../../crates/episcience-core/src/synthesis/skill.rs) (`SynthesisSkill`)
- Registry: [`crates/episcience-core/src/synthesis/skills/mod.rs`](../../crates/episcience-core/src/synthesis/skills/mod.rs) (`load_by_name`)
- EpiClaw caller-side trigger: [epiclaw-host PR #15](https://github.com/tylorsama/epiclaw-host/pull/15)
- Walkthrough: [`05-workflows.md` Workflow D](05-workflows.md#workflow-d--epiclaw-arxiv-scan--literature-synthesis)
- Glossary entries: [literature skill](04-glossary.md#literature-skill), [code_review skill](04-glossary.md#code_review-skill), [registry_diff skill](04-glossary.md#registry_diff-skill), [synthesis_skill (task field)](04-glossary.md#synthesis_skill-task-field)

---

## 14.  Paper-novelty backend

This builds on §9 (novelty assessment) and §13 (`LiteratureSkill`). The default `InternalNoveltyBackend` scores a candidate against prior `complete` syntheses that share at least one cluster member. That signal is right for most skills — a synthesis is novel insofar as no prior synthesis covers the same nodes — but it under-penalises a literature scan that pulls in well-known papers nobody has previously synthesised over. `PaperNoveltyBackend` adds a second signal: similarity against prior DOI-labeled kernel `claims`. The two scores are combined by taking the worse: `score = min(internal_score, 1.0 - top_doi_similarity)`. Both sources have to agree the candidate is novel for the combined score to be high.

Dispatch is by `skill_name`. The synthesis-job handler picks the backend at job-pickup time:

| `skill_name` | Novelty backend | `novelty_backend` column value |
| --- | --- | --- |
| `"literature"` | `PaperNoveltyBackend` | `"paper_novelty"` |
| anything else (`"baseline"`, `"lab_notebook"`, `"code_review"`, `"registry_diff"`, unknown) | `InternalNoveltyBackend` (unchanged) | `"internal_prior_syntheses"` |

The dispatch is name-equality, not skill-property — a future skill that should opt into paper-novelty has to be added to the match arm alongside `"literature"`. The decision was deliberate: novelty backend choice is an orchestration concern, not a skill capability, so it lives next to the worker rather than on the trait.

The algorithm:

1. Run the internal backend exactly as `InternalNoveltyBackend::score` would. This yields `internal_score`, the top-5 prior-synthesis neighbours, and the internal rationale.
2. Embed the candidate narrative (full text, not the internal backend's "head" heuristic — the DOI claims being compared against are themselves full claim contents, so full-text is closer to apples-to-apples).
3. Iterate kernel `claims` rows where `'doi' = ANY(labels)` and `embedding IS NOT NULL`, computing cosine similarity against the candidate embedding. Keep the maximum as `top_doi_similarity`.
4. Combine: `score = min(internal_score.score, (1.0 - top_doi_similarity).clamp(0.0, 1.0))`. The `clamp` guards against floating-point drift pushing cosine slightly above 1.0.
5. Surface both numbers in the `rationale` (`"internal_syntheses 0.83; top_doi_similarity 0.21; combined 0.79"`) so post-hoc inspection sees both. The `neighbours` array carries the internal backend's prior-synthesis neighbours verbatim — DOI matches don't fit the `NoveltyNeighbour` shape (which expects a `synthesis_id`) and a future schema bump would be needed to surface them as first-class neighbour rows.

The **empty-corpus property** matters operationally. DOI labels on kernel claims are seeded by upstream ingestion, not by episcience. On a kernel with no DOI-labeled claims, the SQL returns zero rows, `top_doi_similarity` stays at 0.0, `(1.0 - top_doi_similarity)` is 1.0, and `min(internal_score, 1.0) = internal_score`. The backend is then behaviourally equivalent to `InternalNoveltyBackend` modulo the `name()` string and the rationale wording — no behaviour break, no need for a feature flag, and a kernel that later starts seeding DOI labels gets the new signal automatically.

The embedder dependency is the one place this backend is stricter than the internal one. An embedder failure on Stage 7's candidate-narrative embedding step is fatal here (returned as `NoveltyError::Unavailable`) — without the candidate's vector there is nothing to score against, and silently returning 0.0 would falsely report "no DOI overlap." A novelty-backend failure remains non-fatal at the pipeline level (§9): the synthesis row still moves to `complete` with `novelty_score = NULL`, just as it does when `InternalNoveltyBackend` errors.

**See also:**
- Backend impl: [`crates/episcience-db/src/synthesis/novelty_backend_paper.rs`](../../crates/episcience-db/src/synthesis/novelty_backend_paper.rs) (`PaperNoveltyBackend::score`, `find_top_doi_claim_similarity`)
- Trait: [`crates/episcience-core/src/synthesis/novelty.rs`](../../crates/episcience-core/src/synthesis/novelty.rs) (`NoveltyBackend`)
- Default backend baseline: [`crates/episcience-db/src/synthesis/novelty_backend_internal.rs`](../../crates/episcience-db/src/synthesis/novelty_backend_internal.rs) (`InternalNoveltyBackend`)
- Glossary entries: [paper-novelty backend](04-glossary.md#paper-novelty-backend), [novelty backend](04-glossary.md#novelty-backend), [novelty score](04-glossary.md#novelty-score)

---

## 15.  Review-bot read-side tooling

This builds on §5 (countersignatures) and §13.2 (`CodeReviewSkill`). The Phase 8 surface is intentionally *read-only* — two read-path additions that let an operator-built review-bot find unreviewed candidate syntheses and check whether a given claim already carries an approved countersignature. The bot itself is not shipped code; it is an operator recipe described in [`05-workflows.md` Workflow E](05-workflows.md#workflow-e--countersign-as-merge-gate-review-bot). What ships here is the two read tools the recipe relies on.

**`list_syntheses` (HTTP + MCP) gains a `skill_name` filter.** The existing list endpoint already supported `status` filtering plus pagination via `limit`/`offset`; the addition is an optional `skill_name` query parameter so a caller can ask for, e.g., `?skill_name=code_review&status=complete` and get only the candidates of a single kind. The MCP `list_syntheses` tool mirrors the HTTP shape. The filter is server-side SQL (`WHERE skill_name = $1` when set), not a post-fetch trim, so it stays cheap as the table grows. Without the filter, a review-bot would have to fetch every recent complete synthesis and partition client-side — operable, but it scales badly past a few weeks of runs.

**`list_countersignatures` is now an MCP tool.** Previously the only way to read countersignatures was the HTTP `GET /api/v1/eln/claims/{claim_id}/countersignatures` route, which returns raw byte arrays (`Vec<u8>` serialised as JSON `u8` arrays) for `content_hash` and `signature` — fine for the verify endpoint, awkward for MCP consumers. The new MCP tool hex-encodes `content_hash`, `signature`, and the signer's `public_key` (looked up from the `agents` table per row) and returns a `CountersignatureView` array. The HTTP route is intentionally left unchanged — it has callers that depend on the byte-array shape — so the MCP tool is purely additive. Auth is the same as the HTTP route (no extra gating; countersignatures are conceptually public attestations).

Both surfaces are deliberately read-only. The write tool the bot uses — `countersign` — already existed (Phase 8 MCP write parity, in §5's `See also`). The split keeps the read additions reversible: removing them in a future cleanup does not affect any write semantics. The bot's actual implementation — a 5-minute Claude scheduled task that polls `list_syntheses(skill_name="code_review", status="complete")`, replays the `CodeReviewSkill` rubric against each unsigned narrative, and calls `countersign(claim_id, signature_meaning="approved", ...)` on accept — is the operator-side recipe and lives in `05-workflows.md` Workflow E. The episcience server does not know whether anyone is running such a bot; it just exposes the tools the bot needs.

**See also:**
- HTTP filter addition: [`crates/episcience-api/src/routes/syntheses.rs`](../../crates/episcience-api/src/routes/syntheses.rs) (`ListQuery::skill_name`)
- MCP `list_countersignatures`: [`crates/episcience-api/src/mcp/list_countersignatures.rs`](../../crates/episcience-api/src/mcp/list_countersignatures.rs) (`handle`, `CountersignatureView`)
- MCP registration: [`crates/episcience-api/src/mcp/mod.rs`](../../crates/episcience-api/src/mcp/mod.rs), [`crates/episcience-api/src/mcp/queries.rs`](../../crates/episcience-api/src/mcp/queries.rs)
- Recipe: [`05-workflows.md` Workflow E](05-workflows.md#workflow-e--countersign-as-merge-gate-review-bot)
- Glossary entries: [review-bot](04-glossary.md#review-bot), [countersign-as-merge-gate](04-glossary.md#countersign-as-merge-gate)
