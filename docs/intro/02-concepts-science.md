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
