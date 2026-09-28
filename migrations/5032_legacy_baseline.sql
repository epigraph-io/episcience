-- 5032_legacy_baseline.sql -- EpiScience's own tables, consolidated.
--
-- WHAT: the DDL of the 14 EpiScience-owned tables (samples, sample_claims,
-- protocols, blobs, countersignatures, syntheses and its ten derived/control
-- tables) with their indexes, constraints and their own triggers, exactly as
-- the legacy files under migrations/legacy/ leave them. Every object is
-- `public.`-qualified: `episcience-migrate` runs with
-- `search_path = episcience_meta` so that sqlx's unqualified `_sqlx_migrations`
-- ledger lands in `episcience_meta`, never in the kernel's
-- `public._sqlx_migrations`. The pgvector type and operator class are
-- qualified with the schema the extension lives in (`public`).
--
-- WHY ONE FILE: the legacy files were applied by hand (psql) and interleave
-- with kernel migrations; several of them write KERNEL tables. Replaying them
-- on a database built by the kernel's own `epigraph-migrate` is neither
-- necessary nor safe. This file is the single starting point of EpiScience's
-- ledger. A database that already holds these tables (built the legacy way)
-- is ADOPTED instead of migrated: `episcience-migrate adopt-baseline`
-- fingerprints the live tables, compares them with
-- 5032_legacy_baseline.fingerprint and, on a match, records this version
-- without running it.
--
-- EXCLUDED (kernel-owned or kernel-provided; never recreated here):
--   * 001's `frames` rows, its `experiments` / `experiment_results` tables and
--     its `create_shared_evidence_factor()` + `edges_shared_evidence` trigger on
--     the kernel `edges` table, and its `factors` recompute;
--   * 5000 (kernel `entity_types` rows: the kernel registry already carries the
--     `synthesis` type);
--   * 5001 (`provenance_log.signature_meaning`: EpiScience reads its own
--     `countersignatures.signature_meaning`, never the kernel column);
--   * 5002 (`claims.content_tsv`: the kernel provides it at head 110, kernel
--     migration 050).
--
-- KERNEL OBJECTS REFERENCED: `public.agents`, `public.claims` (foreign keys),
-- `public.update_updated_at_column()` (the samples / protocols `updated_at`
-- triggers, as on every legacy database), `public.vector`.
--
-- Plain CREATE (no IF NOT EXISTS), on purpose: running this file against a
-- database that already has the tables fails and rolls back rather than
-- recording 5032 over a schema it did not build. Use adopt-baseline there.
--
-- CHECK constraints over a `character varying` column keep the SOURCE form
-- the legacy files used (`col IN (...)`), not the form a schema dump prints
-- (`(col)::text = ANY ((ARRAY[...])::text[])`): re-parsing the dump form
-- stores a different expression tree, so `pg_get_constraintdef` (which the
-- adopt fingerprint compares) would render it differently from a legacy
-- database. `syntheses.visibility` is the exception: 5032 writes its CHECK as
-- an in-place `text` -> `character varying(16)` conversion renders it.

CREATE TABLE public.blobs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    filename text NOT NULL,
    mime_type character varying(255) NOT NULL,
    size_bytes bigint NOT NULL,
    content_hash bytea NOT NULL,
    uploader_id uuid NOT NULL,
    sample_id uuid,
    labels text[] DEFAULT '{}'::text[] NOT NULL,
    properties jsonb DEFAULT '{}'::jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT blobs_content_hash_length CHECK ((octet_length(content_hash) = 32)),
    CONSTRAINT blobs_filename_not_empty CHECK ((length(TRIM(BOTH FROM filename)) > 0)),
    CONSTRAINT blobs_size_positive CHECK ((size_bytes > 0))
);

CREATE TABLE public.countersignatures (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    claim_id uuid NOT NULL,
    signer_id uuid NOT NULL,
    signature_meaning character varying(50) NOT NULL,
    content_hash bytea NOT NULL,
    signature bytea NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    prev_signature_hash bytea,
    signature_version smallint DEFAULT 1 NOT NULL,
    CONSTRAINT countersignatures_signature_meaning_check CHECK (signature_meaning IN ('witnessed', 'approved', 'reviewed', 'certified', 'countersigned')),
    CONSTRAINT cs_content_hash_length CHECK ((octet_length(content_hash) = 32)),
    CONSTRAINT cs_prev_hash_length CHECK (((prev_signature_hash IS NULL) OR (octet_length(prev_signature_hash) = 32))),
    CONSTRAINT cs_signature_length CHECK ((octet_length(signature) = 64))
);

CREATE TABLE public.episcience_worker_state (
    worker_id text NOT NULL,
    last_event_id text,
    last_event_ts timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE public.protocols (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    title text NOT NULL,
    version integer DEFAULT 1 NOT NULL,
    authored_by uuid NOT NULL,
    steps jsonb DEFAULT '[]'::jsonb NOT NULL,
    equipment text[] DEFAULT '{}'::text[],
    safety_notes text,
    supersedes uuid,
    labels text[] DEFAULT '{}'::text[] NOT NULL,
    properties jsonb DEFAULT '{}'::jsonb NOT NULL,
    content_hash bytea NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    sections jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT protocols_content_hash_length CHECK ((octet_length(content_hash) = 32)),
    CONSTRAINT protocols_root_version_is_one CHECK (((supersedes IS NOT NULL) OR (version = 1))),
    CONSTRAINT protocols_title_not_empty CHECK ((length(TRIM(BOTH FROM title)) > 0))
);

CREATE TABLE public.sample_claims (
    sample_id uuid NOT NULL,
    claim_id uuid NOT NULL,
    relationship character varying(30) DEFAULT 'observation'::character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT sample_claims_relationship_check CHECK (relationship IN ('observation', 'measurement', 'characterization', 'preparation_note'))
);

CREATE TABLE public.samples (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    sample_type character varying(50) NOT NULL,
    status character varying(30) DEFAULT 'prepared'::character varying NOT NULL,
    parent_sample_id uuid,
    prepared_by uuid NOT NULL,
    preparation_date timestamp with time zone DEFAULT now() NOT NULL,
    expiry_date timestamp with time zone,
    storage_location text,
    quantity_value double precision,
    quantity_unit character varying(30),
    hazard_info jsonb DEFAULT '{}'::jsonb,
    labels text[] DEFAULT '{}'::text[] NOT NULL,
    properties jsonb DEFAULT '{}'::jsonb NOT NULL,
    content_hash bytea NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT samples_content_hash_length CHECK ((octet_length(content_hash) = 32)),
    CONSTRAINT samples_name_not_empty CHECK ((length(TRIM(BOTH FROM name)) > 0)),
    CONSTRAINT samples_quantity_pair CHECK (((quantity_value IS NULL) = (quantity_unit IS NULL))),
    CONSTRAINT samples_sample_type_check CHECK (sample_type IN ('biological', 'chemical', 'material', 'composite', 'workflow_run')),
    CONSTRAINT samples_status_check CHECK (status IN ('prepared', 'in_use', 'consumed', 'disposed', 'archived'))
);

CREATE TABLE public.syntheses (
    id uuid NOT NULL,
    query text NOT NULL,
    agent_id uuid NOT NULL,
    status text NOT NULL,
    parent_synthesis_id uuid,
    narrative text,
    narrative_format text,
    subgraph_snapshot jsonb NOT NULL,
    clustering_method text NOT NULL,
    llm_provider text NOT NULL,
    llm_model text NOT NULL,
    llm_call_count integer DEFAULT 0 NOT NULL,
    prereq_synthesis_ids uuid[],
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    completed_at timestamp with time zone,
    stale_since timestamp with time zone,
    stale_reason text,
    content_hash bytea NOT NULL,
    visibility character varying(16) DEFAULT 'private'::text NOT NULL,
    failure_reason text,
    skill_name text DEFAULT 'baseline'::text NOT NULL,
    verifier_outcome jsonb,
    verifier_attempts smallint DEFAULT 0 NOT NULL,
    novelty_score jsonb,
    novelty_backend text,
    refinement_temperature jsonb,
    autonomy_level text,
    CONSTRAINT syntheses_autonomy_level_check CHECK ((autonomy_level = ANY (ARRAY['co_pilot'::text, 'autopilot'::text, 'autonomous'::text]))),
    CONSTRAINT syntheses_check CHECK (((status = 'complete'::text) = (narrative IS NOT NULL))),
    CONSTRAINT syntheses_check1 CHECK (((status = 'complete'::text) = (completed_at IS NOT NULL))),
    CONSTRAINT syntheses_check2 CHECK (((stale_since IS NULL) = (stale_reason IS NULL))),
    CONSTRAINT syntheses_clustering_method_check CHECK ((clustering_method = 'signed_louvain'::text)),
    CONSTRAINT syntheses_content_hash_check CHECK ((octet_length(content_hash) = 32)),
    CONSTRAINT syntheses_narrative_format_check CHECK (((narrative_format IS NULL) OR (narrative_format = 'markdown'::text))),
    CONSTRAINT syntheses_skill_name_known CHECK ((skill_name = ANY (ARRAY['baseline'::text, 'lab_notebook'::text, 'literature'::text, 'code_review'::text, 'registry_diff'::text]))),
    CONSTRAINT syntheses_stale_reason_check CHECK (((stale_reason IS NULL) OR (stale_reason = ANY (ARRAY['belief_drift'::text, 'new_contradiction'::text, 'claim_superseded'::text, 'frame_changed'::text, 'edge_revoked'::text])))),
    CONSTRAINT syntheses_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'running'::text, 'verifying'::text, 'complete'::text, 'failed'::text, 'deleted'::text, 'rejected'::text]))),
    CONSTRAINT syntheses_visibility_check CHECK (((visibility)::text = ANY (ARRAY['private'::text, 'shared'::text, 'public'::text])))
);

COMMENT ON COLUMN public.syntheses.failure_reason IS 'Reason set by SynthesisRepository::mark_failed when a synthesis transitions to status=failed; preserves stage error text for ops debugging.';

CREATE TABLE public.synthesis_claim_membership (
    synthesis_id uuid NOT NULL,
    claim_id uuid NOT NULL
);

CREATE TABLE public.synthesis_clusters (
    id uuid NOT NULL,
    synthesis_id uuid NOT NULL,
    cluster_index integer NOT NULL,
    title text NOT NULL,
    summary text NOT NULL,
    member_claim_ids uuid[] NOT NULL,
    support_count integer NOT NULL,
    contradict_count integer NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT synthesis_clusters_check CHECK (((support_count >= 0) AND (contradict_count >= 0))),
    CONSTRAINT synthesis_clusters_member_claim_ids_check CHECK ((cardinality(member_claim_ids) > 0)),
    CONSTRAINT synthesis_clusters_title_check CHECK ((length(title) <= 200))
);

CREATE TABLE public.synthesis_embeddings (
    synthesis_id uuid NOT NULL,
    embedding public.vector(1536) NOT NULL,
    embedding_model text NOT NULL,
    embedding_input text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT synthesis_embeddings_embedding_input_check CHECK ((embedding_input = ANY (ARRAY['narrative_head'::text, 'title_plus_query'::text, 'summary_concat'::text])))
);

CREATE TABLE public.synthesis_jobs (
    id uuid NOT NULL,
    job_type text DEFAULT 'synthesis'::text NOT NULL,
    payload jsonb NOT NULL,
    state text DEFAULT 'queued'::text NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    max_attempts integer DEFAULT 3 NOT NULL,
    scheduled_at timestamp with time zone DEFAULT now() NOT NULL,
    started_at timestamp with time zone,
    completed_at timestamp with time zone,
    last_error text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT synthesis_jobs_state_check CHECK ((state = ANY (ARRAY['queued'::text, 'running'::text, 'complete'::text, 'failed'::text, 'retry'::text])))
);

CREATE TABLE public.synthesis_provo_edges (
    synthesis_id uuid NOT NULL,
    predicate text NOT NULL,
    target_kind text NOT NULL,
    target_id uuid NOT NULL,
    written_at timestamp with time zone,
    epigraph_edge_id uuid,
    attempt_count integer DEFAULT 0 NOT NULL,
    last_error text,
    CONSTRAINT synthesis_provo_edges_predicate_check CHECK ((predicate = ANY (ARRAY['WAS_DERIVED_FROM'::text, 'REFINES'::text, 'COMPOSED_OF'::text, 'ATTRIBUTED_TO'::text]))),
    CONSTRAINT synthesis_provo_edges_target_kind_check CHECK ((target_kind = ANY (ARRAY['claim'::text, 'synthesis'::text, 'agent'::text, 'workflow'::text])))
);

CREATE TABLE public.synthesis_shares (
    synthesis_id uuid NOT NULL,
    shared_with_agent_id uuid NOT NULL,
    shared_by_agent_id uuid NOT NULL,
    granted_at timestamp with time zone DEFAULT now() NOT NULL,
    permission text DEFAULT 'read'::text NOT NULL,
    CONSTRAINT synthesis_shares_permission_check CHECK ((permission = 'read'::text))
);

CREATE TABLE public.synthesis_staleness_events (
    id uuid NOT NULL,
    synthesis_id uuid NOT NULL,
    detected_at timestamp with time zone DEFAULT now() NOT NULL,
    trigger text NOT NULL,
    affected_claim_ids uuid[] NOT NULL,
    detail jsonb,
    CONSTRAINT synthesis_staleness_events_trigger_check CHECK ((trigger = ANY (ARRAY['belief_drift'::text, 'new_contradiction'::text, 'claim_superseded'::text, 'frame_changed'::text, 'edge_revoked'::text])))
);

ALTER TABLE ONLY public.blobs
    ADD CONSTRAINT blobs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.countersignatures
    ADD CONSTRAINT countersignatures_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.countersignatures
    ADD CONSTRAINT cs_unique_signer_claim UNIQUE (claim_id, signer_id, signature_meaning);

ALTER TABLE ONLY public.episcience_worker_state
    ADD CONSTRAINT episcience_worker_state_pkey PRIMARY KEY (worker_id);

ALTER TABLE ONLY public.protocols
    ADD CONSTRAINT protocols_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.sample_claims
    ADD CONSTRAINT sample_claims_pkey PRIMARY KEY (sample_id, claim_id);

ALTER TABLE ONLY public.samples
    ADD CONSTRAINT samples_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.syntheses
    ADD CONSTRAINT syntheses_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.synthesis_claim_membership
    ADD CONSTRAINT synthesis_claim_membership_pkey PRIMARY KEY (synthesis_id, claim_id);

ALTER TABLE ONLY public.synthesis_clusters
    ADD CONSTRAINT synthesis_clusters_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.synthesis_clusters
    ADD CONSTRAINT synthesis_clusters_synthesis_id_cluster_index_key UNIQUE (synthesis_id, cluster_index);

ALTER TABLE ONLY public.synthesis_embeddings
    ADD CONSTRAINT synthesis_embeddings_pkey PRIMARY KEY (synthesis_id);

ALTER TABLE ONLY public.synthesis_jobs
    ADD CONSTRAINT synthesis_jobs_pkey PRIMARY KEY (id);

ALTER TABLE ONLY public.synthesis_provo_edges
    ADD CONSTRAINT synthesis_provo_edges_pkey PRIMARY KEY (synthesis_id, predicate, target_kind, target_id);

ALTER TABLE ONLY public.synthesis_shares
    ADD CONSTRAINT synthesis_shares_pkey PRIMARY KEY (synthesis_id, shared_with_agent_id);

ALTER TABLE ONLY public.synthesis_staleness_events
    ADD CONSTRAINT synthesis_staleness_events_pkey PRIMARY KEY (id);

CREATE INDEX idx_blobs_created ON public.blobs USING btree (created_at DESC);

CREATE INDEX idx_blobs_hash ON public.blobs USING btree (content_hash);

CREATE INDEX idx_blobs_labels ON public.blobs USING gin (labels);

CREATE INDEX idx_blobs_sample ON public.blobs USING btree (sample_id) WHERE (sample_id IS NOT NULL);

CREATE INDEX idx_blobs_uploader ON public.blobs USING btree (uploader_id);

CREATE INDEX idx_cs_claim ON public.countersignatures USING btree (claim_id);

CREATE INDEX idx_cs_created ON public.countersignatures USING btree (created_at DESC);

CREATE INDEX idx_cs_prev_hash ON public.countersignatures USING btree (prev_signature_hash) WHERE (prev_signature_hash IS NOT NULL);

CREATE INDEX idx_cs_signer ON public.countersignatures USING btree (signer_id);

CREATE INDEX idx_protocols_authored_by ON public.protocols USING btree (authored_by);

CREATE INDEX idx_protocols_created_at ON public.protocols USING btree (created_at DESC);

CREATE INDEX idx_protocols_labels ON public.protocols USING gin (labels);

CREATE INDEX idx_protocols_supersedes ON public.protocols USING btree (supersedes) WHERE (supersedes IS NOT NULL);

CREATE UNIQUE INDEX idx_protocols_supersedes_version ON public.protocols USING btree (supersedes, version) WHERE (supersedes IS NOT NULL);

CREATE INDEX idx_sample_claims_claim ON public.sample_claims USING btree (claim_id);

CREATE INDEX idx_samples_created_at ON public.samples USING btree (created_at DESC);

CREATE INDEX idx_samples_labels ON public.samples USING gin (labels);

CREATE INDEX idx_samples_parent ON public.samples USING btree (parent_sample_id) WHERE (parent_sample_id IS NOT NULL);

CREATE INDEX idx_samples_prepared_by ON public.samples USING btree (prepared_by);

CREATE INDEX idx_samples_properties ON public.samples USING gin (properties);

CREATE INDEX idx_samples_status ON public.samples USING btree (status);

CREATE INDEX idx_samples_type ON public.samples USING btree (sample_type);

CREATE INDEX syntheses_agent_created_idx ON public.syntheses USING btree (agent_id, created_at DESC);

CREATE INDEX syntheses_stale_idx ON public.syntheses USING btree (stale_since) WHERE (stale_since IS NOT NULL);

CREATE INDEX syntheses_status_idx ON public.syntheses USING btree (status) WHERE (status = ANY (ARRAY['pending'::text, 'running'::text]));

CREATE INDEX synthesis_claim_membership_claim_idx ON public.synthesis_claim_membership USING btree (claim_id);

CREATE INDEX synthesis_claim_membership_synthesis_idx ON public.synthesis_claim_membership USING btree (synthesis_id);

CREATE INDEX synthesis_clusters_synthesis_idx ON public.synthesis_clusters USING btree (synthesis_id);

CREATE INDEX synthesis_embeddings_hnsw_idx ON public.synthesis_embeddings USING hnsw (embedding public.vector_cosine_ops);

CREATE INDEX synthesis_jobs_state_scheduled_idx ON public.synthesis_jobs USING btree (state, scheduled_at) WHERE (state = ANY (ARRAY['queued'::text, 'retry'::text]));

CREATE INDEX synthesis_provo_edges_pending_idx ON public.synthesis_provo_edges USING btree (synthesis_id) WHERE (written_at IS NULL);

CREATE INDEX synthesis_shares_recipient_idx ON public.synthesis_shares USING btree (shared_with_agent_id);

CREATE INDEX synthesis_staleness_events_synthesis_detected_idx ON public.synthesis_staleness_events USING btree (synthesis_id, detected_at DESC);

CREATE TRIGGER protocols_updated_at BEFORE UPDATE ON public.protocols FOR EACH ROW EXECUTE FUNCTION public.update_updated_at_column();

CREATE TRIGGER samples_updated_at BEFORE UPDATE ON public.samples FOR EACH ROW EXECUTE FUNCTION public.update_updated_at_column();

ALTER TABLE ONLY public.blobs
    ADD CONSTRAINT blobs_sample_id_fkey FOREIGN KEY (sample_id) REFERENCES public.samples(id) ON DELETE SET NULL;

ALTER TABLE ONLY public.blobs
    ADD CONSTRAINT blobs_uploader_id_fkey FOREIGN KEY (uploader_id) REFERENCES public.agents(id) ON DELETE RESTRICT;

ALTER TABLE ONLY public.countersignatures
    ADD CONSTRAINT countersignatures_claim_id_fkey FOREIGN KEY (claim_id) REFERENCES public.claims(id) ON DELETE RESTRICT;

ALTER TABLE ONLY public.countersignatures
    ADD CONSTRAINT countersignatures_signer_id_fkey FOREIGN KEY (signer_id) REFERENCES public.agents(id) ON DELETE RESTRICT;

ALTER TABLE ONLY public.protocols
    ADD CONSTRAINT protocols_authored_by_fkey FOREIGN KEY (authored_by) REFERENCES public.agents(id) ON DELETE RESTRICT;

ALTER TABLE ONLY public.protocols
    ADD CONSTRAINT protocols_supersedes_fkey FOREIGN KEY (supersedes) REFERENCES public.protocols(id);

ALTER TABLE ONLY public.sample_claims
    ADD CONSTRAINT sample_claims_claim_id_fkey FOREIGN KEY (claim_id) REFERENCES public.claims(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.sample_claims
    ADD CONSTRAINT sample_claims_sample_id_fkey FOREIGN KEY (sample_id) REFERENCES public.samples(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.samples
    ADD CONSTRAINT samples_parent_sample_id_fkey FOREIGN KEY (parent_sample_id) REFERENCES public.samples(id) ON DELETE RESTRICT;

ALTER TABLE ONLY public.samples
    ADD CONSTRAINT samples_prepared_by_fkey FOREIGN KEY (prepared_by) REFERENCES public.agents(id) ON DELETE RESTRICT;

ALTER TABLE ONLY public.syntheses
    ADD CONSTRAINT syntheses_parent_synthesis_id_fkey FOREIGN KEY (parent_synthesis_id) REFERENCES public.syntheses(id);

ALTER TABLE ONLY public.synthesis_claim_membership
    ADD CONSTRAINT synthesis_claim_membership_synthesis_id_fkey FOREIGN KEY (synthesis_id) REFERENCES public.syntheses(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.synthesis_clusters
    ADD CONSTRAINT synthesis_clusters_synthesis_id_fkey FOREIGN KEY (synthesis_id) REFERENCES public.syntheses(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.synthesis_embeddings
    ADD CONSTRAINT synthesis_embeddings_synthesis_id_fkey FOREIGN KEY (synthesis_id) REFERENCES public.syntheses(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.synthesis_jobs
    ADD CONSTRAINT synthesis_jobs_id_fkey FOREIGN KEY (id) REFERENCES public.syntheses(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.synthesis_provo_edges
    ADD CONSTRAINT synthesis_provo_edges_synthesis_id_fkey FOREIGN KEY (synthesis_id) REFERENCES public.syntheses(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.synthesis_shares
    ADD CONSTRAINT synthesis_shares_synthesis_id_fkey FOREIGN KEY (synthesis_id) REFERENCES public.syntheses(id) ON DELETE CASCADE;

ALTER TABLE ONLY public.synthesis_staleness_events
    ADD CONSTRAINT synthesis_staleness_events_synthesis_id_fkey FOREIGN KEY (synthesis_id) REFERENCES public.syntheses(id) ON DELETE CASCADE;
