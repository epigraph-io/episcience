SELECT public.episcience_assert_kernel_contract(1);

-- 5035_tenancy_columns_contract.sql -- tenancy columns, CONTRACT step.
--
-- Runs after the one-shot re-own (episcience_maint_backfill_owners, 5034):
--   1. the legacy vocabulary becomes the kernel's (`private`/`shared` ->
--      `group`), BEFORE any trigger exists;
--   2. every derived row with no pair takes its parent's (fully determined,
--      not an ownership decision: the new binary writes derived rows without
--      a pair until this migration installs the inherit triggers);
--   3. legacy countersignatures record their signer as the principal that
--      recorded them;
--   4. an ownerless ROOT row, or a job without a principal, refuses the
--      migration (the re-own was not run);
--   5. the pair becomes mandatory: `('public','group')` only, no default, no
--      world or seed owner, NOT NULL; `countersigned_by` and `principal_id`
--      NOT NULL;
--   6. the row guards (SECURITY INVOKER, `SET search_path = public, pg_temp`,
--      fired by name order) and the one DEFINER, the propagation of a
--      parent's pair to its children.
--
-- Row guards are INVOKER on purpose: they must see the SESSION (its
-- principal, its groups, its row security), and a definer would make every
-- check pass as its owner. The only definer is the propagation, which must
-- update child rows the session may not be able to see.
--
-- One explicit statement per table (migration lint: no loops over names).

-- ─── 1. Vocabulary ──────────────────────────────────────────────────────────

UPDATE public.syntheses SET visibility = 'group' WHERE visibility IN ('private', 'shared');

-- ─── 2. Derived rows take their parent's pair ───────────────────────────────

UPDATE public.synthesis_clusters x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.syntheses p
 WHERE p.id = x.synthesis_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.synthesis_embeddings x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.syntheses p
 WHERE p.id = x.synthesis_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.synthesis_staleness_events x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.syntheses p
 WHERE p.id = x.synthesis_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.synthesis_provo_edges x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.syntheses p
 WHERE p.id = x.synthesis_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.synthesis_claim_membership x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.syntheses p
 WHERE p.id = x.synthesis_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.synthesis_jobs x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.syntheses p
 WHERE p.id = x.id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.sample_claims x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.samples p
 WHERE p.id = x.sample_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);
UPDATE public.blobs x SET owner_group_id = p.owner_group_id, visibility = p.visibility
  FROM public.samples p
 WHERE p.id = x.sample_id AND p.owner_group_id IS NOT NULL
   AND (x.owner_group_id IS NULL OR x.visibility IS NULL);

-- ─── 3. Legacy countersignatures: the signer recorded them ──────────────────

UPDATE public.countersignatures SET countersigned_by = signer_id WHERE countersigned_by IS NULL;

-- ─── 4. Nothing left without an owner ───────────────────────────────────────

DO $assert$
DECLARE
    n bigint;
BEGIN
    SELECT count(*) INTO n FROM public.syntheses WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % syntheses rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.synthesis_clusters WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % synthesis_clusters rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.synthesis_embeddings WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % synthesis_embeddings rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.synthesis_staleness_events WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % synthesis_staleness_events rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.synthesis_provo_edges WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % synthesis_provo_edges rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.synthesis_claim_membership WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % synthesis_claim_membership rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.synthesis_jobs WHERE owner_group_id IS NULL OR visibility IS NULL OR principal_id IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % synthesis_jobs rows have no ownership pair or no principal', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.samples WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % samples rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.sample_claims WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % sample_claims rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.protocols WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % protocols rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.blobs WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % blobs rows have no ownership pair', n
            USING HINT = 'run episcience-maint backfill-owners (the one-shot re-own) first';
    END IF;
    SELECT count(*) INTO n FROM public.countersignatures WHERE owner_group_id IS NULL OR visibility IS NULL;
    IF n > 0 THEN
        RAISE EXCEPTION '5035: % countersignatures rows have no ownership pair', n
            USING HINT = 'assign their owner out of band (the re-own refuses them), then re-run';
    END IF;
END $assert$;

-- ─── 5. The pair is mandatory ───────────────────────────────────────────────
-- `<t>_group_needs_real_group`: no EpiScience row is owned by the kernel's
-- world or seed sentinel, whatever its visibility (stricter than the
-- kernel's own claims rule, which admits a public world row).

ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_visibility_check;
ALTER TABLE public.syntheses ALTER COLUMN visibility DROP DEFAULT;
ALTER TABLE public.syntheses ADD CONSTRAINT syntheses_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.synthesis_clusters ADD CONSTRAINT synthesis_clusters_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.synthesis_embeddings ADD CONSTRAINT synthesis_embeddings_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.synthesis_staleness_events ADD CONSTRAINT synthesis_staleness_events_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.synthesis_provo_edges ADD CONSTRAINT synthesis_provo_edges_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.synthesis_claim_membership ADD CONSTRAINT synthesis_claim_membership_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.synthesis_jobs ADD CONSTRAINT synthesis_jobs_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.samples ADD CONSTRAINT samples_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.sample_claims ADD CONSTRAINT sample_claims_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.protocols ADD CONSTRAINT protocols_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.blobs ADD CONSTRAINT blobs_visibility_check
    CHECK (visibility IN ('public', 'group'));
ALTER TABLE public.countersignatures ADD CONSTRAINT countersignatures_visibility_check
    CHECK (visibility IN ('public', 'group'));

ALTER TABLE public.syntheses ADD CONSTRAINT syntheses_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.synthesis_clusters ADD CONSTRAINT synthesis_clusters_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.synthesis_embeddings ADD CONSTRAINT synthesis_embeddings_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.synthesis_staleness_events ADD CONSTRAINT synthesis_staleness_events_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.synthesis_provo_edges ADD CONSTRAINT synthesis_provo_edges_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.synthesis_claim_membership ADD CONSTRAINT synthesis_claim_membership_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.synthesis_jobs ADD CONSTRAINT synthesis_jobs_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.samples ADD CONSTRAINT samples_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.sample_claims ADD CONSTRAINT sample_claims_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.protocols ADD CONSTRAINT protocols_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.blobs ADD CONSTRAINT blobs_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));
ALTER TABLE public.countersignatures ADD CONSTRAINT countersignatures_group_needs_real_group
    CHECK (owner_group_id <> ALL (ARRAY['00000000-0000-0000-0000-000000000000', '00000000-0000-0000-0000-00000000dead']::uuid[]));

ALTER TABLE public.syntheses ALTER COLUMN owner_group_id SET NOT NULL;
ALTER TABLE public.synthesis_clusters ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.synthesis_embeddings ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.synthesis_staleness_events ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.synthesis_provo_edges ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.synthesis_claim_membership ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.synthesis_jobs ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL,
    ALTER COLUMN principal_id SET NOT NULL;
ALTER TABLE public.samples ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.sample_claims ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.protocols ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.blobs ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL;
ALTER TABLE public.countersignatures ALTER COLUMN owner_group_id SET NOT NULL, ALTER COLUMN visibility SET NOT NULL,
    ALTER COLUMN countersigned_by SET NOT NULL;

-- The narrowing a completion may apply (the publish rule below) and the
-- narrowing sweep record themselves in the staleness vocabulary.
ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_stale_reason_check;
ALTER TABLE public.syntheses ADD CONSTRAINT syntheses_stale_reason_check
    CHECK (stale_reason IS NULL OR stale_reason = ANY (ARRAY['belief_drift'::text, 'new_contradiction'::text,
        'claim_superseded'::text, 'frame_changed'::text, 'edge_revoked'::text, 'input_narrowed'::text]));
ALTER TABLE public.synthesis_staleness_events DROP CONSTRAINT synthesis_staleness_events_trigger_check;
ALTER TABLE public.synthesis_staleness_events ADD CONSTRAINT synthesis_staleness_events_trigger_check
    CHECK (trigger = ANY (ARRAY['belief_drift'::text, 'new_contradiction'::text, 'claim_superseded'::text,
        'frame_changed'::text, 'edge_revoked'::text, 'input_narrowed'::text]));

-- ─── 6a. Publishability (INVOKER helpers) ───────────────────────────────────
-- A synthesis may be public only when every member claim is visible to the
-- session and public, its parent (if any) is public, and every prerequisite
-- exists and is public. A sample: every attached claim visible and public,
-- and its parent (if any) public. Hidden counts as non-public.

CREATE FUNCTION public.episcience_synthesis_is_publishable(p_id uuid, p_parent uuid, p_prereqs uuid[])
RETURNS boolean
LANGUAGE sql STABLE SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
    SELECT NOT EXISTS (SELECT 1 FROM synthesis_claim_membership m
                         LEFT JOIN claims c ON c.id = m.claim_id
                        WHERE m.synthesis_id = p_id
                          AND (c.id IS NULL OR c.visibility::text <> 'public'))
       AND (p_parent IS NULL
            OR EXISTS (SELECT 1 FROM syntheses p WHERE p.id = p_parent AND p.visibility = 'public'))
       AND NOT EXISTS (SELECT 1 FROM unnest(coalesce(p_prereqs, ARRAY[]::uuid[])) x(id)
                         LEFT JOIN syntheses p ON p.id = x.id
                        WHERE p.id IS NULL OR p.visibility <> 'public')
$fn$;

CREATE FUNCTION public.episcience_sample_is_publishable(p_id uuid, p_parent uuid)
RETURNS boolean
LANGUAGE sql STABLE SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
    SELECT NOT EXISTS (SELECT 1 FROM sample_claims sc
                         LEFT JOIN claims c ON c.id = sc.claim_id
                        WHERE sc.sample_id = p_id
                          AND (c.id IS NULL OR c.visibility::text <> 'public'))
       AND (p_parent IS NULL
            OR EXISTS (SELECT 1 FROM samples p WHERE p.id = p_parent AND p.visibility = 'public'))
$fn$;

-- ─── 6b. Row guards (INVOKER) ───────────────────────────────────────────────

-- tenancy_10_require (ROOT tables): parent arms first, then "declared, pass",
-- else 23502. A parent is read on the SESSION's row security, so a parent the
-- session cannot see is 23503 (reported like a missing one).
--   syntheses: a refinement of a NON-public parent must be owned by the
--     parent's group (42501); an undeclared refinement copies the parent's pair.
--   samples: a child of a GROUP sample must be ('group', <parent group>)
--     (42501); an undeclared child copies the parent's pair.
--   protocols: the superseded protocol must be in the session's writable
--     groups (42501; a new version of someone else's protocol is a fork);
--     an undeclared supersede copies its pair.
--   blobs: a blob on a sample ALWAYS takes the sample's pair (derived).
CREATE FUNCTION public.episcience_root_require_tenancy()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_owner uuid;
    v_vis   text;
BEGIN
    IF TG_TABLE_NAME = 'syntheses' THEN
        IF NEW.parent_synthesis_id IS NOT NULL THEN
            SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis
              FROM syntheses p WHERE p.id = NEW.parent_synthesis_id;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'parent synthesis % is not visible', NEW.parent_synthesis_id
                    USING ERRCODE = '23503';
            END IF;
            IF NEW.owner_group_id IS NULL AND NEW.visibility IS NULL THEN
                NEW.owner_group_id := v_owner;
                NEW.visibility := v_vis;
            ELSIF v_vis <> 'public' AND NEW.owner_group_id IS DISTINCT FROM v_owner THEN
                RAISE EXCEPTION 'a refinement of a non-public synthesis must be owned by the parent''s group'
                    USING ERRCODE = '42501';
            END IF;
        END IF;
    ELSIF TG_TABLE_NAME = 'samples' THEN
        IF NEW.parent_sample_id IS NOT NULL THEN
            SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis
              FROM samples p WHERE p.id = NEW.parent_sample_id;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'parent sample % is not visible', NEW.parent_sample_id
                    USING ERRCODE = '23503';
            END IF;
            IF NEW.owner_group_id IS NULL AND NEW.visibility IS NULL THEN
                NEW.owner_group_id := v_owner;
                NEW.visibility := v_vis;
            ELSIF v_vis = 'group'
                  AND (NEW.owner_group_id IS DISTINCT FROM v_owner OR NEW.visibility IS DISTINCT FROM 'group') THEN
                RAISE EXCEPTION 'a child of a group sample must be (group, the parent''s group)'
                    USING ERRCODE = '42501';
            END IF;
        END IF;
    ELSIF TG_TABLE_NAME = 'protocols' THEN
        IF NEW.supersedes IS NOT NULL THEN
            SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis
              FROM protocols p WHERE p.id = NEW.supersedes;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'superseded protocol % is not visible', NEW.supersedes
                    USING ERRCODE = '23503';
            END IF;
            IF NOT public.episcience_session_is_privileged()
               AND NOT (v_owner = ANY ((SELECT public.epigraph_writable_groups())::uuid[])) THEN
                RAISE EXCEPTION 'superseding a protocol needs write access to its owner group; publish a new protocol instead'
                    USING ERRCODE = '42501';
            END IF;
            IF NEW.owner_group_id IS NULL AND NEW.visibility IS NULL THEN
                NEW.owner_group_id := v_owner;
                NEW.visibility := v_vis;
            END IF;
        END IF;
    ELSIF TG_TABLE_NAME = 'blobs' THEN
        IF NEW.sample_id IS NOT NULL THEN
            SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis
              FROM samples p WHERE p.id = NEW.sample_id;
            IF NOT FOUND THEN
                RAISE EXCEPTION 'sample % is not visible', NEW.sample_id
                    USING ERRCODE = '23503';
            END IF;
            NEW.owner_group_id := v_owner;
            NEW.visibility := v_vis;
            RETURN NEW;
        END IF;
    END IF;
    IF NEW.owner_group_id IS NULL OR NEW.visibility IS NULL THEN
        RAISE EXCEPTION '% row declares no ownership pair', TG_TABLE_NAME
            USING ERRCODE = '23502',
                  HINT = 'a root insert declares owner_group_id and visibility';
    END IF;
    RETURN NEW;
END $fn$;

-- tenancy_10_inherit (DERIVED tables): the parent is read on the session's
-- row security (missing or invisible: 23503) and the row's pair is ALWAYS the
-- parent's, whatever the insert declared. TG_ARGV[0] names the column that
-- holds the parent id.
CREATE FUNCTION public.episcience_inherit_from_synthesis()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_parent uuid := (to_jsonb(NEW) ->> TG_ARGV[0])::uuid;
    v_owner  uuid;
    v_vis    text;
BEGIN
    SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis FROM syntheses p WHERE p.id = v_parent;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'synthesis % is not visible', v_parent USING ERRCODE = '23503';
    END IF;
    NEW.owner_group_id := v_owner;
    NEW.visibility := v_vis;
    RETURN NEW;
END $fn$;

CREATE FUNCTION public.episcience_inherit_from_sample()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_parent uuid := (to_jsonb(NEW) ->> TG_ARGV[0])::uuid;
    v_owner  uuid;
    v_vis    text;
BEGIN
    SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis FROM samples p WHERE p.id = v_parent;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'sample % is not visible', v_parent USING ERRCODE = '23503';
    END IF;
    NEW.owner_group_id := v_owner;
    NEW.visibility := v_vis;
    RETURN NEW;
END $fn$;

-- tenancy_15_author: the author column (TG_ARGV[0]) is the session's
-- principal. A non-privileged session must carry one (42501 otherwise); a
-- NULL column becomes it; a different value is 42501; any UPDATE naming the
-- column is 42501. A privileged session (migration, maintenance, the current
-- superuser runtime) passes: its writes bind the author themselves.
CREATE FUNCTION public.episcience_author_is_principal()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_principal uuid;
    v_value     uuid;
BEGIN
    IF public.episcience_session_is_privileged() THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'the author column % is immutable', TG_ARGV[0] USING ERRCODE = '42501';
    END IF;
    v_principal := public.epigraph_principal_id();
    IF v_principal IS NULL THEN
        RAISE EXCEPTION 'an EpiScience write needs a principal' USING ERRCODE = '42501';
    END IF;
    v_value := (to_jsonb(NEW) ->> TG_ARGV[0])::uuid;
    IF v_value IS NULL THEN
        NEW := jsonb_populate_record(NEW, jsonb_build_object(TG_ARGV[0], v_principal));
    ELSIF v_value <> v_principal THEN
        RAISE EXCEPTION 'the author column % must be the session principal', TG_ARGV[0]
            USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $fn$;

-- tenancy_20_claim_guard (CLAIM-ATTACH tables): the claim must be visible to
-- the session (23503 otherwise) and either public or owned by the row's
-- group (42501). A countersignature of a GROUP claim must itself be
-- `('group', <claim group>)` with that group writable by the session.
CREATE FUNCTION public.episcience_claim_attach_guard()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_vis   text;
    v_owner uuid;
BEGIN
    SELECT c.visibility::text, c.owner_group_id INTO v_vis, v_owner
      FROM claims c WHERE c.id = NEW.claim_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'claim % is not visible', NEW.claim_id USING ERRCODE = '23503';
    END IF;
    IF v_vis = 'public' THEN
        RETURN NEW;
    END IF;
    IF NEW.owner_group_id IS DISTINCT FROM v_owner THEN
        RAISE EXCEPTION 'a group claim attaches only to a row owned by the claim''s group'
            USING ERRCODE = '42501';
    END IF;
    IF TG_TABLE_NAME = 'countersignatures' THEN
        IF NEW.visibility IS DISTINCT FROM 'group' THEN
            RAISE EXCEPTION 'a countersignature of a group claim is a group row'
                USING ERRCODE = '42501';
        END IF;
        IF NOT public.episcience_session_is_privileged()
           AND NOT (v_owner = ANY ((SELECT public.epigraph_writable_groups())::uuid[])) THEN
            RAISE EXCEPTION 'countersigning a group claim needs write access to its group'
                USING ERRCODE = '42501';
        END IF;
    END IF;
    RETURN NEW;
END $fn$;

-- tenancy_20_principal (synthesis_jobs): a non-privileged session's job acts
-- as the session principal, whatever the insert declared (NULL principal:
-- 42501), and the payload's agent_id is set to it. A privileged session must
-- supply the principal (23502 otherwise); the payload follows it too.
CREATE FUNCTION public.episcience_job_principal()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_principal uuid;
BEGIN
    IF public.episcience_session_is_privileged() THEN
        IF NEW.principal_id IS NULL THEN
            RAISE EXCEPTION 'a synthesis job needs an explicit principal_id on a privileged session'
                USING ERRCODE = '23502';
        END IF;
    ELSE
        v_principal := public.epigraph_principal_id();
        IF v_principal IS NULL THEN
            RAISE EXCEPTION 'a synthesis job needs a principal' USING ERRCODE = '42501';
        END IF;
        NEW.principal_id := v_principal;
    END IF;
    NEW.payload := jsonb_set(coalesce(NEW.payload, '{}'::jsonb), '{agent_id}', to_jsonb(NEW.principal_id));
    RETURN NEW;
END $fn$;

-- tenancy_30_owner_immutable (every tenancy table) and
-- tenancy_30_derived_pinned (derived tables; blobs while on a sample): the
-- owner, and a derived row's pair, change only on a privileged session (the
-- propagation below, the one-shot re-own).
CREATE FUNCTION public.episcience_owner_immutable()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
BEGIN
    IF NOT public.episcience_session_is_privileged() THEN
        RAISE EXCEPTION 'the owner of % rows is immutable', TG_TABLE_NAME USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $fn$;

CREATE FUNCTION public.episcience_derived_pinned()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
BEGIN
    IF NOT public.episcience_session_is_privileged() THEN
        RAISE EXCEPTION 'a % row takes its parent''s ownership pair; change the parent', TG_TABLE_NAME
            USING ERRCODE = '42501';
    END IF;
    RETURN NEW;
END $fn$;

-- tenancy_40_widening_guard (syntheses, samples; group -> public only): needs
-- the transaction-local interlock `episcience.allow_widen = 'yes'` AND a
-- publishable row. No privileged exemption.
CREATE FUNCTION public.episcience_block_widening()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
BEGIN
    IF coalesce(current_setting('episcience.allow_widen', true), '') <> 'yes' THEN
        RAISE EXCEPTION 'widening a % row to public needs the widening interlock', TG_TABLE_NAME
            USING ERRCODE = '42501',
                  HINT = 'set episcience.allow_widen = yes for the transaction';
    END IF;
    IF TG_TABLE_NAME = 'syntheses' THEN
        IF NOT public.episcience_synthesis_is_publishable(NEW.id, NEW.parent_synthesis_id, NEW.prereq_synthesis_ids) THEN
            RAISE EXCEPTION 'synthesis % cannot be public: a member claim, its parent or a prerequisite is not public', NEW.id
                USING ERRCODE = '42501';
        END IF;
    ELSE
        IF NOT public.episcience_sample_is_publishable(NEW.id, NEW.parent_sample_id) THEN
            RAISE EXCEPTION 'sample % cannot be public: an attached claim or its parent is not public', NEW.id
                USING ERRCODE = '42501';
        END IF;
    END IF;
    RETURN NEW;
END $fn$;

-- tenancy_45_publish_rule (syntheses): a PUBLIC synthesis reaching
-- `complete` that is not publishable completes as `group`, marked
-- `input_narrowed` (the owner sees why it did not come out public).
CREATE FUNCTION public.episcience_publish_rule()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
BEGIN
    IF NOT public.episcience_synthesis_is_publishable(NEW.id, NEW.parent_synthesis_id, NEW.prereq_synthesis_ids) THEN
        NEW.visibility := 'group';
        NEW.stale_since := coalesce(NEW.stale_since, now());
        NEW.stale_reason := coalesce(NEW.stale_reason, 'input_narrowed');
    END IF;
    RETURN NEW;
END $fn$;

-- ─── 6c. The propagation (the one DEFINER of this migration) ────────────────
-- AFTER UPDATE, FOR EACH STATEMENT, with transition tables and NO column list
-- (Postgres refuses a column list together with transition tables). Returns
-- at once when no parent's pair changed; otherwise every child's pair becomes
-- its parent's, and each child table's updated count must equal the count of
-- children that differed (a row the owner cannot reach would otherwise be
-- skipped silently). Maintenance-owned so it reaches children the session
-- may not see; `epigraph_definer_bypass()` admits it to the row guards.
CREATE FUNCTION public.episcience_propagate_parent_tenancy()
RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_expected bigint;
    v_actual   bigint;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM prev p JOIN changed c ON c.id = p.id
                    WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)) THEN
        RETURN NULL;
    END IF;

    IF TG_TABLE_NAME = 'syntheses' THEN
        SELECT count(*) INTO v_expected FROM synthesis_clusters x
          JOIN changed c ON c.id = x.synthesis_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE synthesis_clusters x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.synthesis_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to synthesis_clusters: % expected, % updated', v_expected, v_actual;
        END IF;

        SELECT count(*) INTO v_expected FROM synthesis_embeddings x
          JOIN changed c ON c.id = x.synthesis_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE synthesis_embeddings x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.synthesis_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to synthesis_embeddings: % expected, % updated', v_expected, v_actual;
        END IF;

        SELECT count(*) INTO v_expected FROM synthesis_staleness_events x
          JOIN changed c ON c.id = x.synthesis_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE synthesis_staleness_events x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.synthesis_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to synthesis_staleness_events: % expected, % updated', v_expected, v_actual;
        END IF;

        SELECT count(*) INTO v_expected FROM synthesis_provo_edges x
          JOIN changed c ON c.id = x.synthesis_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE synthesis_provo_edges x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.synthesis_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to synthesis_provo_edges: % expected, % updated', v_expected, v_actual;
        END IF;

        SELECT count(*) INTO v_expected FROM synthesis_claim_membership x
          JOIN changed c ON c.id = x.synthesis_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE synthesis_claim_membership x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.synthesis_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to synthesis_claim_membership: % expected, % updated', v_expected, v_actual;
        END IF;

        SELECT count(*) INTO v_expected FROM synthesis_jobs x
          JOIN changed c ON c.id = x.id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE synthesis_jobs x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to synthesis_jobs: % expected, % updated', v_expected, v_actual;
        END IF;
    ELSE
        SELECT count(*) INTO v_expected FROM sample_claims x
          JOIN changed c ON c.id = x.sample_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE sample_claims x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.sample_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to sample_claims: % expected, % updated', v_expected, v_actual;
        END IF;

        SELECT count(*) INTO v_expected FROM blobs x
          JOIN changed c ON c.id = x.sample_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        UPDATE blobs x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.sample_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to blobs: % expected, % updated', v_expected, v_actual;
        END IF;

        -- Child samples: only a child that carries the parent's OLD pair
        -- follows (the child of a group sample, which the require guard
        -- pinned to that pair, or a same-owner child of a public one); its
        -- own propagation fires in turn. A child of a PUBLIC sample is owned
        -- on its own and is never re-owned here; if the parent became
        -- `group`, such a child would sit under a group parent in another
        -- pair, so the change is refused instead.
        SELECT count(*) INTO v_expected FROM samples x
          JOIN changed c ON c.id = x.parent_sample_id JOIN prev p ON p.id = c.id
         WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS NOT DISTINCT FROM (p.owner_group_id, p.visibility);
        UPDATE samples x SET owner_group_id = c.owner_group_id, visibility = c.visibility
          FROM changed c JOIN prev p ON p.id = c.id
         WHERE x.parent_sample_id = c.id
           AND (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
           AND (x.owner_group_id, x.visibility) IS NOT DISTINCT FROM (p.owner_group_id, p.visibility);
        GET DIAGNOSTICS v_actual = ROW_COUNT;
        IF v_actual <> v_expected THEN
            RAISE EXCEPTION 'propagation to child samples: % expected, % updated', v_expected, v_actual;
        END IF;
        IF EXISTS (SELECT 1 FROM samples x
                     JOIN changed c ON c.id = x.parent_sample_id JOIN prev p ON p.id = c.id
                    WHERE (c.owner_group_id, c.visibility) IS DISTINCT FROM (p.owner_group_id, p.visibility)
                      AND c.visibility = 'group'
                      AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility)) THEN
            RAISE EXCEPTION 'a child sample owned by another group would sit under a group sample'
                USING ERRCODE = '42501',
                      HINT = 'detach or re-own the child sample first';
        END IF;
    END IF;
    RETURN NULL;
END $fn$;

ALTER FUNCTION public.episcience_propagate_parent_tenancy() OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_propagate_parent_tenancy() FROM PUBLIC;

-- ─── 6d. The triggers (BEFORE ROW triggers fire in name order) ──────────────

-- syntheses (ROOT)
CREATE TRIGGER tenancy_10_require BEFORE INSERT ON public.syntheses
    FOR EACH ROW EXECUTE FUNCTION public.episcience_root_require_tenancy();
CREATE TRIGGER tenancy_15_author BEFORE INSERT OR UPDATE OF agent_id ON public.syntheses
    FOR EACH ROW EXECUTE FUNCTION public.episcience_author_is_principal('agent_id');
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.syntheses
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();
CREATE TRIGGER tenancy_40_widening_guard BEFORE UPDATE OF visibility ON public.syntheses
    FOR EACH ROW WHEN (OLD.visibility = 'group' AND NEW.visibility = 'public')
    EXECUTE FUNCTION public.episcience_block_widening();
CREATE TRIGGER tenancy_45_publish_rule BEFORE UPDATE OF status ON public.syntheses
    FOR EACH ROW WHEN (NEW.status = 'complete' AND OLD.status IS DISTINCT FROM 'complete' AND NEW.visibility = 'public')
    EXECUTE FUNCTION public.episcience_publish_rule();
CREATE TRIGGER tenancy_90_propagate AFTER UPDATE ON public.syntheses
    REFERENCING OLD TABLE AS prev NEW TABLE AS changed
    FOR EACH STATEMENT EXECUTE FUNCTION public.episcience_propagate_parent_tenancy();

-- the six DERIVED(syntheses) tables
CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.synthesis_clusters
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_synthesis('synthesis_id');
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.synthesis_clusters
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.synthesis_clusters
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.synthesis_embeddings
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_synthesis('synthesis_id');
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.synthesis_embeddings
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.synthesis_embeddings
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.synthesis_staleness_events
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_synthesis('synthesis_id');
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.synthesis_staleness_events
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.synthesis_staleness_events
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.synthesis_provo_edges
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_synthesis('synthesis_id');
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.synthesis_provo_edges
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.synthesis_provo_edges
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.synthesis_claim_membership
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_synthesis('synthesis_id');
CREATE TRIGGER tenancy_20_claim_guard BEFORE INSERT ON public.synthesis_claim_membership
    FOR EACH ROW EXECUTE FUNCTION public.episcience_claim_attach_guard();
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.synthesis_claim_membership
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.synthesis_claim_membership
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.synthesis_jobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_synthesis('id');
CREATE TRIGGER tenancy_20_principal BEFORE INSERT ON public.synthesis_jobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_job_principal();
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.synthesis_jobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.synthesis_jobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

-- samples (ROOT) and sample_claims (DERIVED)
CREATE TRIGGER tenancy_10_require BEFORE INSERT ON public.samples
    FOR EACH ROW EXECUTE FUNCTION public.episcience_root_require_tenancy();
CREATE TRIGGER tenancy_15_author BEFORE INSERT OR UPDATE OF prepared_by ON public.samples
    FOR EACH ROW EXECUTE FUNCTION public.episcience_author_is_principal('prepared_by');
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.samples
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();
CREATE TRIGGER tenancy_40_widening_guard BEFORE UPDATE OF visibility ON public.samples
    FOR EACH ROW WHEN (OLD.visibility = 'group' AND NEW.visibility = 'public')
    EXECUTE FUNCTION public.episcience_block_widening();
CREATE TRIGGER tenancy_90_propagate AFTER UPDATE ON public.samples
    REFERENCING OLD TABLE AS prev NEW TABLE AS changed
    FOR EACH STATEMENT EXECUTE FUNCTION public.episcience_propagate_parent_tenancy();

CREATE TRIGGER tenancy_10_inherit BEFORE INSERT ON public.sample_claims
    FOR EACH ROW EXECUTE FUNCTION public.episcience_inherit_from_sample('sample_id');
CREATE TRIGGER tenancy_20_claim_guard BEFORE INSERT ON public.sample_claims
    FOR EACH ROW EXECUTE FUNCTION public.episcience_claim_attach_guard();
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.sample_claims
    FOR EACH ROW EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.sample_claims
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

-- protocols (ROOT)
CREATE TRIGGER tenancy_10_require BEFORE INSERT ON public.protocols
    FOR EACH ROW EXECUTE FUNCTION public.episcience_root_require_tenancy();
CREATE TRIGGER tenancy_15_author BEFORE INSERT OR UPDATE OF authored_by ON public.protocols
    FOR EACH ROW EXECUTE FUNCTION public.episcience_author_is_principal('authored_by');
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.protocols
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

-- blobs (ROOT without a sample, DERIVED(samples) with one)
CREATE TRIGGER tenancy_10_require BEFORE INSERT ON public.blobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_root_require_tenancy();
CREATE TRIGGER tenancy_15_author BEFORE INSERT OR UPDATE OF uploader_id ON public.blobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_author_is_principal('uploader_id');
CREATE TRIGGER tenancy_30_derived_pinned BEFORE UPDATE OF owner_group_id, visibility ON public.blobs
    FOR EACH ROW WHEN (OLD.sample_id IS NOT NULL)
    EXECUTE FUNCTION public.episcience_derived_pinned();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.blobs
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();

-- countersignatures (CLAIM-ATTACH, append-only in the RLS migration)
CREATE TRIGGER tenancy_15_author BEFORE INSERT OR UPDATE OF countersigned_by ON public.countersignatures
    FOR EACH ROW EXECUTE FUNCTION public.episcience_author_is_principal('countersigned_by');
CREATE TRIGGER tenancy_20_claim_guard BEFORE INSERT ON public.countersignatures
    FOR EACH ROW EXECUTE FUNCTION public.episcience_claim_attach_guard();
CREATE TRIGGER tenancy_30_owner_immutable BEFORE UPDATE OF owner_group_id ON public.countersignatures
    FOR EACH ROW EXECUTE FUNCTION public.episcience_owner_immutable();
