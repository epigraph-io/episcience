# Glossary (science-specific)

Vocabulary for episcience's experimental loop layer. For kernel terms (claim, edge, agent, BetP, etc.) see the [EpiGraph glossary](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/04-glossary.md).

---

## blob

A content-addressed binary artifact — gel image, microscope frame, instrument trace, attached PDF — stored on the filesystem at `EPISCIENCE_BLOB_DIR/{hash[0:2]}/{hash[2:4]}/{hash}.blob` with metadata (filename, MIME type, size, uploader, optional `sample_id`) in the `blobs` table. Duplicate uploads are deduplicated by BLAKE3 `content_hash`, so the same raw image referenced from two experiments stores one copy. Blobs are how raw data enters the experimental loop; they are typically referenced by the synthesis claims or experiment results that interpret them. [see 02-concepts-science.md §4 (Blobs)](02-concepts-science.md#4--blobs)

## countersignature

A second agent's Ed25519 signature attesting to an existing claim with one of five meanings: `witnessed`, `approved`, `reviewed`, `certified`, or `countersigned`. Each row pins `claim_id`, `signer_id`, the 32-byte BLAKE3 `content_hash` it signed, the 64-byte signature, and (since migration 5010) a `prev_signature_hash` that chains countersignatures for tamper-evident review trails. Countersignatures are how the lab ELN layer encodes the "two-person rule" — a synthesis claim or measurement is not "approved" until a second qualified agent has signed it. [see 02-concepts-science.md §5 (Countersignatures)](02-concepts-science.md#5--countersignatures)

## experiment

The run-time instantiation of a protocol against one or more samples to test a hypothesis — conceptually, a single execution of "apply protocol P to sample S and observe outcome O." A schema exists (`experiments` table, extracted from EpiGraphV2 migration 049) with `hypothesis_id`, `protocol`, `status` (`designed → running → collecting → analyzing → complete | failed`), and timestamps, but there is no `experiments` API route in episcience today. In practice, the user-facing surface uses synthesis claims that reference sample and protocol IDs; an `experiments` endpoint is planned but not present. [see 02-concepts-science.md §1 (Experiments and experiment-results)](02-concepts-science.md#1--experiments-and-experiment-results)

## experiment-result

The observed outcome of an experiment — the bridge from raw measurements to a claim that propagates belief. The `experiment_results` table records `experiment_id`, `data_source` (`manual | simulation | instrument | literature | computed`), `raw_measurements` (JSONB), `measurement_count`, `effective_random_error`, and `processed_data`, all linked back to the parent experiment via `ON DELETE CASCADE`. As with `experiment`, the surface today is synthesis claims referencing samples and blobs rather than a dedicated `experiment_results` route. [see 02-concepts-science.md §1 (Experiments and experiment-results)](02-concepts-science.md#1--experiments-and-experiment-results)

## PROV-O

The W3C Provenance Ontology — a standard vocabulary for describing how things came to be. The `synthesis_provo_edges` table allows four predicates: `WAS_DERIVED_FROM` (the only one taken directly from the W3C PROV-O standard, corresponding to `wasDerivedFrom`), plus episcience-specific predicates `REFINES`, `COMPOSED_OF`, and `ATTRIBUTED_TO` (the last is inspired by PROV-O's attribution concept but is not a standard PROV-O relation as named). These are kept in a separate table from the kernel's epistemic edge types (supports, refutes, refines, etc.) so dependency provenance does not conflate with belief-bearing edges. [see 02-concepts-science.md §6 (Synthesis claims and PROV-O edges)](02-concepts-science.md#6--synthesis-claims-and-prov-o-edges)

## protocol

A versioned, traceable lab SOP — the recipe an experiment instantiates. The `protocols` table stores `title`, integer `version`, an ordered `steps` JSONB array (each step may carry duration, temperature, notes), an `equipment` list, optional `safety_notes`, and a `supersedes` self-reference for the version chain; a BLAKE3 `content_hash` over the steps lets clients detect drift. Every experiment must reference the specific protocol version used, so a later edit produces a new protocol rather than mutating the cited one. [see 02-concepts-science.md §3 (Protocols)](02-concepts-science.md#3--protocols)

## sample

A tracked physical material — DNA origami batch, protein construct, substrate, reagent, aliquot — with chain-of-custody from preparation through disposal. The `samples` table tracks `sample_type`, a `status` lifecycle (`prepared → in_use → consumed | disposed → archived`), `parent_sample_id` for aliquots and derivatives, `prepared_by`, quantity, storage location, hazard info, and a BLAKE3 `content_hash`; the `sample_claims` junction links a sample to EpiGraph claims as `observation`, `measurement`, `characterization`, or `preparation_note`. Samples are how the physical world enters the graph: nothing is observed without a sample to observe it from. [see 02-concepts-science.md §2 (Samples)](02-concepts-science.md#2--samples)

## synthesis claim

A narrative claim generated by clustering a subgraph and asking an LLM to summarize it — episcience's current stand-in for "experiment result" in user-facing flows. A `syntheses` row pins the originating `query`, the captured `subgraph_snapshot` (claim IDs, edge IDs, belief intervals, traversal config), the clustering method (`signed_louvain`), LLM provider/model, the generated `narrative`, a BLAKE3 `content_hash`, and a staleness field that can fire on `belief_drift`, `new_contradiction`, `claim_superseded`, `frame_changed`, or `edge_revoked`. Synthesis claims are linked back into the kernel via `synthesis_provo_edges` (PROV-O) and via membership edges to the claims they summarize. [see 02-concepts-science.md §6 (Synthesis claims and PROV-O edges)](02-concepts-science.md#6--synthesis-claims-and-prov-o-edges)
