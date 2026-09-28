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
-- row so a later `episcience-migrate run` re-applies 5035. Every row keeps
-- its owner (the backfill's reverse is a separate, manifest-driven step,
-- possible only once this has run). The one data change: a synthesis marked
-- stale `input_narrowed` (a word the pre-5035 vocabulary lacks) loses that
-- mark; it stays `group`, so nothing widens.
--
-- This alone does NOT make the data readable by the previous (E1c) binary:
-- that binary cannot decode `group`. The full rollback order is in
-- RUNBOOK-E1d section 5: stop the E1d units; this script; optionally the
-- backfill reverse; then docs/runbooks/e1c-rollback-vocabulary.sql; only
-- then start the previous binaries.
--
-- Refuses, before changing anything:
--   * while E1e (5036 or 5037) is recorded: run docs/runbooks/e1e-undo.sql
--     first (5037 re-points the publishability helpers this drops, and its
--     sweep calls them);
--   * when a later re-apply of 5035 would itself refuse, i.e. the data holds
--     a row 5035's step 4 rejects: a membership, sample link or
--     countersignature citing a claim that is no longer public and belongs
--     to another group (the normal state once a cited claim is narrowed after
--     it was cited: row security hides such rows, 5035 treats them as written
--     without its guards), or a child sample of a group sample in another
--     pair. Undoing 5035 on such data could not be followed by a re-apply, so
--     it is one-way: roll forward instead.
--
-- The narrowing sweep (E1e) records `input_narrowed` staleness EVENTS, a word
-- the pre-5035 vocabulary lacks: this removes them (the sweep's audit rows in
-- security_events stay, and the syntheses stay `group`).

DO $guard$
DECLARE
    n bigint;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5035 AND success) THEN
        RAISE EXCEPTION '5035 is not recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version IN (5036, 5037)) THEN
        RAISE EXCEPTION 'E1e (5036/5037) is recorded; run docs/runbooks/e1e-undo.sql first';
    END IF;
    -- 5035's step-4 checks, verbatim in substance: each one that would fire
    -- on a re-apply.
    SELECT (SELECT count(*) FROM public.synthesis_claim_membership m JOIN public.claims c ON c.id = m.claim_id
             WHERE c.visibility::text <> 'public' AND c.owner_group_id IS DISTINCT FROM m.owner_group_id)
         + (SELECT count(*) FROM public.sample_claims m JOIN public.claims c ON c.id = m.claim_id
             WHERE c.visibility::text <> 'public' AND c.owner_group_id IS DISTINCT FROM m.owner_group_id)
         + (SELECT count(*) FROM public.countersignatures m JOIN public.claims c ON c.id = m.claim_id
             WHERE c.visibility::text <> 'public'
               AND (c.owner_group_id IS DISTINCT FROM m.owner_group_id OR m.visibility IS DISTINCT FROM 'group'))
         + (SELECT count(*) FROM public.samples x JOIN public.samples p ON p.id = x.parent_sample_id
             WHERE p.visibility = 'group'
               AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility))
      INTO n;
    IF n > 0 THEN
        RAISE EXCEPTION '% rows cite a claim narrowed to another group (or a group parent in another pair); a re-apply of 5035 would refuse them, so this undo would be one-way: nothing changed, roll forward instead', n;
    END IF;
END $guard$;

-- The sweep's staleness events (the audit rows stay).
DELETE FROM public.synthesis_staleness_events WHERE trigger = 'input_narrowed';

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

-- The staleness vocabulary (a narrowed synthesis stays `group`, unmarked).
UPDATE public.syntheses SET stale_since = NULL, stale_reason = NULL WHERE stale_reason = 'input_narrowed';
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
