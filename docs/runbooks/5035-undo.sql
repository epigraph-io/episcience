-- docs/runbooks/5035-undo.sql -- compensating SQL for migration 5035.
--
-- NOT a migration (the ledger is forward-only). Run by the operator, as the
-- migration owner, in ONE transaction, only to back out the contract step:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/5035-undo.sql
--
-- It removes the row guards and the propagation, makes the ownership pair
-- nullable again, restores the expand-step CHECKs (which admit the legacy
-- `private` / `shared` words as well as `group`), and removes the 5035 ledger
-- row so a later `episcience-migrate run` re-applies 5035. It does NOT touch
-- data: every row keeps its owner (the backfill's reverse is a separate,
-- manifest-driven step, possible only once this has run).
--
-- Refuses when a row was narrowed by the completion rule (its stale_reason
-- `input_narrowed` is not in the pre-5035 vocabulary): clear those first.

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5035 AND success) THEN
        RAISE EXCEPTION '5035 is not recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM public.syntheses WHERE stale_reason = 'input_narrowed')
       OR EXISTS (SELECT 1 FROM public.synthesis_staleness_events WHERE trigger = 'input_narrowed') THEN
        RAISE EXCEPTION 'rows carry input_narrowed; clear them before undoing 5035';
    END IF;
END $guard$;

-- Triggers (every one 5035 created).
DROP TRIGGER tenancy_10_require ON public.syntheses;
DROP TRIGGER tenancy_12_parent_pinned ON public.syntheses;
DROP TRIGGER tenancy_15_author ON public.syntheses;
DROP TRIGGER tenancy_30_owner_immutable ON public.syntheses;
DROP TRIGGER tenancy_40_widening_guard ON public.syntheses;
DROP TRIGGER tenancy_45_publish_rule ON public.syntheses;
DROP TRIGGER tenancy_90_propagate ON public.syntheses;
DROP TRIGGER tenancy_10_inherit ON public.synthesis_clusters;
DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_clusters;
DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_clusters;
DROP TRIGGER tenancy_30_owner_immutable ON public.synthesis_clusters;
DROP TRIGGER tenancy_10_inherit ON public.synthesis_embeddings;
DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_embeddings;
DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_embeddings;
DROP TRIGGER tenancy_30_owner_immutable ON public.synthesis_embeddings;
DROP TRIGGER tenancy_10_inherit ON public.synthesis_staleness_events;
DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_staleness_events;
DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_staleness_events;
DROP TRIGGER tenancy_30_owner_immutable ON public.synthesis_staleness_events;
DROP TRIGGER tenancy_10_inherit ON public.synthesis_provo_edges;
DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_provo_edges;
DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_provo_edges;
DROP TRIGGER tenancy_30_owner_immutable ON public.synthesis_provo_edges;
DROP TRIGGER tenancy_10_inherit ON public.synthesis_claim_membership;
DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_claim_membership;
DROP TRIGGER tenancy_20_claim_guard ON public.synthesis_claim_membership;
DROP TRIGGER tenancy_50_narrow_parent ON public.synthesis_claim_membership;
DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_claim_membership;
DROP TRIGGER tenancy_30_owner_immutable ON public.synthesis_claim_membership;
DROP TRIGGER tenancy_10_inherit ON public.synthesis_jobs;
DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_jobs;
DROP TRIGGER tenancy_20_principal ON public.synthesis_jobs;
DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_jobs;
DROP TRIGGER tenancy_30_owner_immutable ON public.synthesis_jobs;
DROP TRIGGER tenancy_10_require ON public.samples;
DROP TRIGGER tenancy_12_parent_pinned ON public.samples;
DROP TRIGGER tenancy_15_author ON public.samples;
DROP TRIGGER tenancy_30_owner_immutable ON public.samples;
DROP TRIGGER tenancy_40_widening_guard ON public.samples;
DROP TRIGGER tenancy_90_propagate ON public.samples;
DROP TRIGGER tenancy_10_inherit ON public.sample_claims;
DROP TRIGGER tenancy_12_parent_pinned ON public.sample_claims;
DROP TRIGGER tenancy_20_claim_guard ON public.sample_claims;
DROP TRIGGER tenancy_30_derived_pinned ON public.sample_claims;
DROP TRIGGER tenancy_30_owner_immutable ON public.sample_claims;
DROP TRIGGER tenancy_10_require ON public.protocols;
DROP TRIGGER tenancy_12_parent_pinned ON public.protocols;
DROP TRIGGER tenancy_15_author ON public.protocols;
DROP TRIGGER tenancy_30_owner_immutable ON public.protocols;
DROP TRIGGER tenancy_10_require ON public.blobs;
DROP TRIGGER tenancy_12_parent_pinned ON public.blobs;
DROP TRIGGER tenancy_15_author ON public.blobs;
DROP TRIGGER tenancy_30_derived_pinned ON public.blobs;
DROP TRIGGER tenancy_30_owner_immutable ON public.blobs;
DROP TRIGGER tenancy_15_author ON public.countersignatures;
DROP TRIGGER tenancy_12_parent_pinned ON public.countersignatures;
DROP TRIGGER tenancy_20_claim_guard ON public.countersignatures;
DROP TRIGGER tenancy_30_owner_immutable ON public.countersignatures;

-- Functions (every one 5035 created).
DROP FUNCTION public.episcience_propagate_parent_tenancy();
DROP FUNCTION public.episcience_parent_pinned();
DROP FUNCTION public.episcience_narrow_on_private_member();
DROP FUNCTION public.episcience_publish_rule();
DROP FUNCTION public.episcience_block_widening();
DROP FUNCTION public.episcience_derived_pinned();
DROP FUNCTION public.episcience_owner_immutable();
DROP FUNCTION public.episcience_job_principal();
DROP FUNCTION public.episcience_claim_attach_guard();
DROP FUNCTION public.episcience_author_is_principal();
DROP FUNCTION public.episcience_inherit_from_sample();
DROP FUNCTION public.episcience_inherit_from_synthesis();
DROP FUNCTION public.episcience_root_require_tenancy();
DROP FUNCTION public.episcience_sample_is_publishable(uuid, uuid);
DROP FUNCTION public.episcience_synthesis_is_publishable(uuid, uuid, uuid[]);

-- The staleness vocabulary.
ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_stale_reason_check;
ALTER TABLE public.syntheses ADD CONSTRAINT syntheses_stale_reason_check
    CHECK (((stale_reason IS NULL) OR (stale_reason = ANY (ARRAY['belief_drift'::text, 'new_contradiction'::text,
        'claim_superseded'::text, 'frame_changed'::text, 'edge_revoked'::text]))));
ALTER TABLE public.synthesis_staleness_events DROP CONSTRAINT synthesis_staleness_events_trigger_check;
ALTER TABLE public.synthesis_staleness_events ADD CONSTRAINT synthesis_staleness_events_trigger_check
    CHECK ((trigger = ANY (ARRAY['belief_drift'::text, 'new_contradiction'::text, 'claim_superseded'::text,
        'frame_changed'::text, 'edge_revoked'::text])));

-- The pair: nullable again, the expand-step visibility CHECK on syntheses,
-- no CHECKs on the other tables' pair.
ALTER TABLE public.syntheses ALTER COLUMN owner_group_id DROP NOT NULL;
ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_visibility_check;
ALTER TABLE public.syntheses ADD CONSTRAINT syntheses_visibility_check
    CHECK (visibility IN ('private', 'shared', 'public', 'group'));
ALTER TABLE public.syntheses ALTER COLUMN visibility SET DEFAULT 'private'::text;
ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_group_needs_real_group;
ALTER TABLE public.synthesis_clusters ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT synthesis_clusters_visibility_check, DROP CONSTRAINT synthesis_clusters_group_needs_real_group;
ALTER TABLE public.synthesis_embeddings ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT synthesis_embeddings_visibility_check, DROP CONSTRAINT synthesis_embeddings_group_needs_real_group;
ALTER TABLE public.synthesis_staleness_events ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT synthesis_staleness_events_visibility_check, DROP CONSTRAINT synthesis_staleness_events_group_needs_real_group;
ALTER TABLE public.synthesis_provo_edges ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT synthesis_provo_edges_visibility_check, DROP CONSTRAINT synthesis_provo_edges_group_needs_real_group;
ALTER TABLE public.synthesis_claim_membership ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT synthesis_claim_membership_visibility_check, DROP CONSTRAINT synthesis_claim_membership_group_needs_real_group;
ALTER TABLE public.synthesis_jobs ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    ALTER COLUMN principal_id DROP NOT NULL,
    DROP CONSTRAINT synthesis_jobs_visibility_check, DROP CONSTRAINT synthesis_jobs_group_needs_real_group;
ALTER TABLE public.samples ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT samples_visibility_check, DROP CONSTRAINT samples_group_needs_real_group;
ALTER TABLE public.sample_claims ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT sample_claims_visibility_check, DROP CONSTRAINT sample_claims_group_needs_real_group;
ALTER TABLE public.protocols ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT protocols_visibility_check, DROP CONSTRAINT protocols_group_needs_real_group;
ALTER TABLE public.blobs ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    DROP CONSTRAINT blobs_visibility_check, DROP CONSTRAINT blobs_group_needs_real_group;
ALTER TABLE public.countersignatures ALTER COLUMN owner_group_id DROP NOT NULL, ALTER COLUMN visibility DROP NOT NULL,
    ALTER COLUMN countersigned_by DROP NOT NULL,
    DROP CONSTRAINT countersignatures_visibility_check, DROP CONSTRAINT countersignatures_group_needs_real_group;

-- The ledger: 5035 is no longer applied.
DELETE FROM episcience_meta._sqlx_migrations WHERE version = 5035;
