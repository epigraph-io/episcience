SELECT public.episcience_assert_kernel_contract(1);

-- 5034_tenancy_columns_expand.sql -- tenancy columns, EXPAND step.
--
-- WHAT: every EpiScience tenancy table gains the kernel's ownership pair
-- (`owner_group_id`, `visibility`), NULLABLE and unenforced; `syntheses`
-- keeps its existing `visibility` column and only widens its vocabulary. New
-- bookkeeping columns: `synthesis_jobs.principal_id` (the principal a job acts
-- as), `syntheses.staleness_checked_at`, `synthesis_provo_edges.deferred_reason`
-- and `countersignatures.countersigned_by` (the principal that recorded the
-- attestation; `signer_id` stays the key holder). The kernel maintenance role
-- gets table privileges on the 14 tables BEFORE the first maintenance-owned
-- definer exists. Then the two one-shot backfill definers.
--
-- WHY EXPAND FIRST: nothing here is enforced. The previous binary keeps
-- working on this schema, the new binary declares every pair it writes, and
-- the legacy rows are re-owned by the audited backfill before 5035 makes the
-- pair mandatory and installs the row guards.
--
-- The 12 tenancy tables: syntheses and its six derived tables
-- (synthesis_clusters, synthesis_embeddings, synthesis_staleness_events,
-- synthesis_provo_edges, synthesis_claim_membership, synthesis_jobs), samples
-- and sample_claims, protocols, blobs, countersignatures. The two frozen
-- tables (synthesis_shares, episcience_worker_state) get no pair.
--
-- One explicit statement per table on purpose (migration lint: a loop over
-- table names is refused).

-- ─── The ownership pair ─────────────────────────────────────────────────────

ALTER TABLE public.syntheses
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT;
ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_visibility_check;
ALTER TABLE public.syntheses ADD CONSTRAINT syntheses_visibility_check
    CHECK (visibility IN ('private', 'shared', 'public', 'group'));

ALTER TABLE public.synthesis_clusters
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.synthesis_embeddings
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.synthesis_staleness_events
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.synthesis_provo_edges
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.synthesis_claim_membership
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.synthesis_jobs
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.samples
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.sample_claims
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.protocols
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.blobs
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);
ALTER TABLE public.countersignatures
    ADD COLUMN owner_group_id uuid REFERENCES public.groups(id) ON DELETE RESTRICT,
    ADD COLUMN visibility character varying(16);

-- ─── Bookkeeping columns ────────────────────────────────────────────────────

ALTER TABLE public.synthesis_jobs ADD COLUMN principal_id uuid;
ALTER TABLE public.syntheses ADD COLUMN staleness_checked_at timestamp with time zone;
ALTER TABLE public.synthesis_provo_edges ADD COLUMN deferred_reason text;
ALTER TABLE public.countersignatures
    ADD COLUMN countersigned_by uuid REFERENCES public.agents(id) ON DELETE RESTRICT;

-- The owner lookups the row guards and the propagation make (5035).
CREATE INDEX syntheses_owner_group_idx ON public.syntheses USING btree (owner_group_id);
CREATE INDEX samples_owner_group_idx ON public.samples USING btree (owner_group_id);
CREATE INDEX protocols_owner_group_idx ON public.protocols USING btree (owner_group_id);
CREATE INDEX blobs_owner_group_idx ON public.blobs USING btree (owner_group_id);

-- ─── Kernel maintenance privileges ──────────────────────────────────────────
-- The kernel's one-shot schema-wide grant to its maintenance role predates
-- these tables on a freshly built database, so the maintenance-owned definers
-- (the backfill below, the propagation and the queue definers later) would be
-- refused without this. Restated idempotently by the RLS migration.

GRANT SELECT, INSERT, UPDATE, DELETE ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs,
    public.countersignatures, public.episcience_worker_state
    TO epigraph_maintenance;

-- ─── The one-shot backfill ──────────────────────────────────────────────────
-- Re-owns the legacy ROOT rows (those with no owner) to the EXISTING personal
-- group of one principal, supplied by the operator at deploy time (never a
-- literal here), and derives every derived row's pair from its parent.
--
--   syntheses          legacy 'public' -> (G, public); 'private'/'shared' -> (G, group)
--   protocols, samples, blobs without a sample -> (G, public)
--   derived rows       := their parent's pair (never G directly: a synthesis a
--                         different principal created during the deploy window
--                         keeps its own owner, and so do its children)
--   synthesis_jobs     principal_id := the principal, for the jobs of the
--                      syntheses this call re-owned; a `complete` job row is
--                      inserted for a re-owned complete synthesis that has
--                      none (every synthesis has exactly one job row)
--
-- Authorship columns are never touched. Only rows with no owner are changed,
-- so a second call changes nothing. Refused: a missing principal; a principal
-- that is operated by another agent (the kernel refuses such a principal's
-- tokens); a principal without a live admin membership in its own personal
-- group (the group is never provisioned here); countersignatures with no
-- owner (a countersignature's owner follows the WRITER's group and the
-- claim's, which only the writing request knows; they are resolved out of
-- band before this runs).
--
-- p_apply = false is a DRY RUN: the same statements run inside a
-- subtransaction that is then rolled back, so the manifest is exactly what an
-- apply would do and nothing persists (the audit rows included). p_apply =
-- true commits them and appends one security_events row per table changed.
--
-- The manifest (one jsonb) lists, per table, every changed row's key, its
-- after-pair and its before-visibility; `episcience_maint_backfill_reverse`
-- consumes it.
CREATE FUNCTION public.episcience_maint_backfill_owners(p_principal uuid, p_apply boolean)
RETURNS TABLE (manifest jsonb)
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_group    uuid;
    v_tables   jsonb := '{}'::jsonb;
    v_counts   jsonb := '{}'::jsonb;
    v_rows     jsonb;
    v_n        bigint;
    v_name     text;
BEGIN
    IF p_principal IS NULL OR p_apply IS NULL THEN
        RAISE EXCEPTION 'episcience backfill: principal and apply flag are both required'
            USING ERRCODE = '22004';
    END IF;
    IF EXISTS (SELECT 1 FROM epigraph_operator_of_author(p_principal)) THEN
        RAISE EXCEPTION 'episcience backfill: principal % is operated by another agent; its tokens are refused, so it cannot own the legacy rows', p_principal
            USING ERRCODE = '55000',
                  HINT = 'pass the principal the operator''s own human token carries';
    END IF;
    SELECT g.id INTO v_group
      FROM groups g
      JOIN group_memberships m
        ON m.group_id = g.id
       AND m.agent_id = p_principal
       AND m.revoked_at IS NULL
       AND m.role = 'admin'
     WHERE g.kind = 'personal'
       AND g.did_key = 'did:epigraph:personal:' || p_principal::text;
    IF v_group IS NULL THEN
        RAISE EXCEPTION 'episcience backfill: principal % has no personal group with a live admin membership', p_principal
            USING ERRCODE = '55000',
                  HINT = 'the backfill never provisions a group; the principal must have signed in once';
    END IF;
    SELECT count(*) INTO v_n FROM countersignatures WHERE owner_group_id IS NULL;
    IF v_n > 0 THEN
        RAISE EXCEPTION 'episcience backfill: % legacy countersignature rows have no owner; this backfill does not assign them', v_n
            USING ERRCODE = '55000',
                  HINT = 'decide their owner out of band before re-running (they attach to kernel claims)';
    END IF;

    BEGIN
        -- ROOT rows first, so the derived rows below can read their parent's pair.
        WITH u AS (
            UPDATE syntheses s
               SET owner_group_id = v_group,
                   visibility = CASE WHEN s.visibility = 'public' THEN 'public' ELSE 'group' END
              FROM (SELECT id, visibility AS before_visibility FROM syntheses
                     WHERE owner_group_id IS NULL) b
             WHERE s.id = b.id
            RETURNING s.id, s.owner_group_id AS after_owner, s.visibility::text AS after_visibility,
                      b.before_visibility::text AS before_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('syntheses', v_rows);

        WITH u AS (
            UPDATE samples s SET owner_group_id = v_group, visibility = 'public'
             WHERE s.owner_group_id IS NULL
            RETURNING s.id, s.owner_group_id AS after_owner, s.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('samples', v_rows);

        WITH u AS (
            UPDATE protocols p SET owner_group_id = v_group, visibility = 'public'
             WHERE p.owner_group_id IS NULL
            RETURNING p.id, p.owner_group_id AS after_owner, p.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('protocols', v_rows);

        -- Blobs: a blob without a sample is a root; one with a sample takes
        -- the sample's pair (after the samples above were re-owned).
        WITH u AS (
            UPDATE blobs b
               SET owner_group_id = coalesce(sm.owner_group_id, v_group),
                   visibility = coalesce(sm.visibility, 'public')
              FROM blobs b0
              LEFT JOIN samples sm ON sm.id = b0.sample_id
             WHERE b.id = b0.id
               AND b.owner_group_id IS NULL
               AND (b0.sample_id IS NULL OR sm.owner_group_id IS NOT NULL)
            RETURNING b.id, b.owner_group_id AS after_owner, b.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('blobs', v_rows);

        -- Derived rows: their parent's pair, whoever owns the parent.
        WITH u AS (
            UPDATE sample_claims x SET owner_group_id = p.owner_group_id, visibility = p.visibility
              FROM samples p
             WHERE p.id = x.sample_id AND x.owner_group_id IS NULL AND p.owner_group_id IS NOT NULL
            RETURNING x.sample_id, x.claim_id, x.owner_group_id AS after_owner, x.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.sample_id, u.claim_id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('sample_claims', v_rows);

        WITH u AS (
            UPDATE synthesis_clusters x SET owner_group_id = p.owner_group_id, visibility = p.visibility
              FROM syntheses p
             WHERE p.id = x.synthesis_id AND x.owner_group_id IS NULL AND p.owner_group_id IS NOT NULL
            RETURNING x.id, x.owner_group_id AS after_owner, x.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('synthesis_clusters', v_rows);

        WITH u AS (
            UPDATE synthesis_embeddings x SET owner_group_id = p.owner_group_id, visibility = p.visibility
              FROM syntheses p
             WHERE p.id = x.synthesis_id AND x.owner_group_id IS NULL AND p.owner_group_id IS NOT NULL
            RETURNING x.synthesis_id, x.owner_group_id AS after_owner, x.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.synthesis_id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('synthesis_embeddings', v_rows);

        WITH u AS (
            UPDATE synthesis_staleness_events x SET owner_group_id = p.owner_group_id, visibility = p.visibility
              FROM syntheses p
             WHERE p.id = x.synthesis_id AND x.owner_group_id IS NULL AND p.owner_group_id IS NOT NULL
            RETURNING x.id, x.owner_group_id AS after_owner, x.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('synthesis_staleness_events', v_rows);

        WITH u AS (
            UPDATE synthesis_provo_edges x SET owner_group_id = p.owner_group_id, visibility = p.visibility
              FROM syntheses p
             WHERE p.id = x.synthesis_id AND x.owner_group_id IS NULL AND p.owner_group_id IS NOT NULL
            RETURNING x.synthesis_id, x.predicate, x.target_kind, x.target_id,
                      x.owner_group_id AS after_owner, x.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.synthesis_id, u.predicate, u.target_kind, u.target_id), '[]'::jsonb)
          INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('synthesis_provo_edges', v_rows);

        WITH u AS (
            UPDATE synthesis_claim_membership x SET owner_group_id = p.owner_group_id, visibility = p.visibility
              FROM syntheses p
             WHERE p.id = x.synthesis_id AND x.owner_group_id IS NULL AND p.owner_group_id IS NOT NULL
            RETURNING x.synthesis_id, x.claim_id, x.owner_group_id AS after_owner, x.visibility::text AS after_visibility
        )
        SELECT coalesce(jsonb_agg(to_jsonb(u) ORDER BY u.synthesis_id, u.claim_id), '[]'::jsonb) INTO v_rows FROM u;
        v_tables := v_tables || jsonb_build_object('synthesis_claim_membership', v_rows);

        -- Jobs: the pair from the synthesis; the principal only for the jobs
        -- of syntheses this call re-owned (their ids are in the manifest).
        WITH reowned AS (
            SELECT (e->>'id')::uuid AS id FROM jsonb_array_elements(v_tables->'syntheses') e
        ), u AS (
            UPDATE synthesis_jobs x
               SET owner_group_id = coalesce(x.owner_group_id, p.owner_group_id),
                   visibility = coalesce(x.visibility, p.visibility),
                   principal_id = CASE WHEN x.principal_id IS NULL AND x.id IN (SELECT id FROM reowned)
                                       THEN p_principal ELSE x.principal_id END
              FROM (SELECT j.id, j.principal_id AS before_principal FROM synthesis_jobs j) b,
                   syntheses p
             WHERE x.id = b.id AND p.id = x.id AND p.owner_group_id IS NOT NULL
               AND (x.owner_group_id IS NULL
                    OR (x.principal_id IS NULL AND x.id IN (SELECT id FROM reowned)))
            RETURNING x.id, x.owner_group_id AS after_owner, x.visibility::text AS after_visibility,
                      x.principal_id AS after_principal, b.before_principal, false AS inserted
        ), ins AS (
            INSERT INTO synthesis_jobs (id, job_type, payload, state, attempts, max_attempts,
                                        scheduled_at, started_at, completed_at, created_at, updated_at,
                                        owner_group_id, visibility, principal_id)
            SELECT s.id, 'synthesis',
                   jsonb_build_object('synthesis_id', s.id, 'query', s.query, 'traversal_config', NULL,
                                      'agent_id', p_principal, 'parent_synthesis_id', s.parent_synthesis_id,
                                      'prereq_synthesis_ids', to_jsonb(coalesce(s.prereq_synthesis_ids, ARRAY[]::uuid[])),
                                      'workflow_run_id', NULL),
                   'complete', 0, 3, s.created_at, s.created_at, s.completed_at, s.created_at, now(),
                   s.owner_group_id, s.visibility, p_principal
              FROM syntheses s
             WHERE s.id IN (SELECT id FROM reowned)
               AND s.status = 'complete'
               AND NOT EXISTS (SELECT 1 FROM synthesis_jobs j WHERE j.id = s.id)
            RETURNING id, owner_group_id AS after_owner, visibility::text AS after_visibility,
                      principal_id AS after_principal, NULL::uuid AS before_principal, true AS inserted
        )
        SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]'::jsonb) INTO v_rows
          FROM (SELECT * FROM u UNION ALL SELECT * FROM ins) a;
        v_tables := v_tables || jsonb_build_object('synthesis_jobs', v_rows);

        FOR v_name IN SELECT jsonb_object_keys(v_tables) LOOP
            v_n := jsonb_array_length(v_tables->v_name);
            v_counts := v_counts || jsonb_build_object(v_name, v_n);
            IF v_n > 0 THEN
                INSERT INTO security_events (event_type, agent_id, success, details)
                VALUES ('episcience.maint.backfill_owners', NULL, true,
                        jsonb_build_object('table', v_name, 'rows', v_n,
                                           'target_group', v_group, 'principal', p_principal,
                                           'applied', p_apply));
            END IF;
        END LOOP;

        IF NOT p_apply THEN
            RAISE EXCEPTION 'episcience backfill dry run' USING ERRCODE = 'E1DRY';
        END IF;
    EXCEPTION WHEN SQLSTATE 'E1DRY' THEN
        -- Every change above is rolled back; the manifest variables are kept.
        NULL;
    END;

    manifest := jsonb_build_object(
        'kind', 'episcience.backfill_owners.v1',
        'principal', p_principal,
        'target_group', v_group,
        'applied', p_apply,
        'counts', v_counts,
        'tables', v_tables);
    RETURN NEXT;
END $fn$;

ALTER FUNCTION public.episcience_maint_backfill_owners(uuid, boolean) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_maint_backfill_owners(uuid, boolean) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_maint_backfill_owners(uuid, boolean) TO episcience_maint_ops;

-- Undo an applied backfill from its manifest, ONLY while the pair is still
-- nullable (before 5035; afterwards it refuses). A row is restored only if its
-- current pair still equals the manifest's after-pair, so a row changed since
-- (re-owned again, or narrowed) is left alone. Jobs: the principal is cleared
-- only where it still equals the manifest's; job rows the backfill inserted
-- are deleted only if still unchanged. One security_events row per table
-- restored. Returns the number of rows restored or deleted.
CREATE FUNCTION public.episcience_maint_backfill_reverse(p_manifest jsonb)
RETURNS integer
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_total   integer := 0;
    v_n       integer;
    v_counts  jsonb := '{}'::jsonb;
    v_name    text;
BEGIN
    IF p_manifest IS NULL OR p_manifest->>'kind' IS DISTINCT FROM 'episcience.backfill_owners.v1' THEN
        RAISE EXCEPTION 'episcience backfill reverse: not a backfill manifest' USING ERRCODE = '22023';
    END IF;
    IF (p_manifest->>'applied')::boolean IS NOT TRUE THEN
        RAISE EXCEPTION 'episcience backfill reverse: the manifest is a dry run; nothing to reverse'
            USING ERRCODE = '22023';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                WHERE a.attrelid = 'public.syntheses'::regclass
                  AND a.attname = 'owner_group_id' AND a.attnotnull) THEN
        RAISE EXCEPTION 'episcience backfill reverse: the ownership pair is already mandatory (5035 applied); reverse is only possible before it'
            USING ERRCODE = '55000';
    END IF;

    UPDATE syntheses s SET owner_group_id = NULL, visibility = r.before_visibility
      FROM jsonb_to_recordset(p_manifest->'tables'->'syntheses')
           AS r(id uuid, after_owner uuid, after_visibility text, before_visibility text)
     WHERE s.id = r.id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('syntheses', v_n);

    UPDATE samples s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'samples') AS r(id uuid, after_owner uuid, after_visibility text)
     WHERE s.id = r.id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('samples', v_n);

    UPDATE protocols s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'protocols') AS r(id uuid, after_owner uuid, after_visibility text)
     WHERE s.id = r.id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('protocols', v_n);

    UPDATE blobs s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'blobs') AS r(id uuid, after_owner uuid, after_visibility text)
     WHERE s.id = r.id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('blobs', v_n);

    UPDATE sample_claims s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'sample_claims')
           AS r(sample_id uuid, claim_id uuid, after_owner uuid, after_visibility text)
     WHERE s.sample_id = r.sample_id AND s.claim_id = r.claim_id
       AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('sample_claims', v_n);

    UPDATE synthesis_clusters s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'synthesis_clusters') AS r(id uuid, after_owner uuid, after_visibility text)
     WHERE s.id = r.id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_clusters', v_n);

    UPDATE synthesis_embeddings s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'synthesis_embeddings')
           AS r(synthesis_id uuid, after_owner uuid, after_visibility text)
     WHERE s.synthesis_id = r.synthesis_id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_embeddings', v_n);

    UPDATE synthesis_staleness_events s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'synthesis_staleness_events')
           AS r(id uuid, after_owner uuid, after_visibility text)
     WHERE s.id = r.id AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_staleness_events', v_n);

    UPDATE synthesis_provo_edges s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'synthesis_provo_edges')
           AS r(synthesis_id uuid, predicate text, target_kind text, target_id uuid,
                after_owner uuid, after_visibility text)
     WHERE s.synthesis_id = r.synthesis_id AND s.predicate = r.predicate
       AND s.target_kind = r.target_kind AND s.target_id = r.target_id
       AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_provo_edges', v_n);

    UPDATE synthesis_claim_membership s SET owner_group_id = NULL, visibility = NULL
      FROM jsonb_to_recordset(p_manifest->'tables'->'synthesis_claim_membership')
           AS r(synthesis_id uuid, claim_id uuid, after_owner uuid, after_visibility text)
     WHERE s.synthesis_id = r.synthesis_id AND s.claim_id = r.claim_id
       AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_claim_membership', v_n);

    -- Jobs the backfill inserted: deleted while still exactly as inserted.
    DELETE FROM synthesis_jobs s
     USING jsonb_to_recordset(p_manifest->'tables'->'synthesis_jobs')
           AS r(id uuid, after_owner uuid, after_visibility text, after_principal uuid, inserted boolean)
     WHERE r.inserted AND s.id = r.id AND s.owner_group_id = r.after_owner
       AND s.visibility = r.after_visibility AND s.principal_id = r.after_principal
       AND s.state = 'complete';
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_jobs_inserted', v_n);

    UPDATE synthesis_jobs s
       SET owner_group_id = NULL, visibility = NULL,
           principal_id = CASE WHEN s.principal_id IS NOT DISTINCT FROM r.after_principal
                               THEN r.before_principal ELSE s.principal_id END
      FROM jsonb_to_recordset(p_manifest->'tables'->'synthesis_jobs')
           AS r(id uuid, after_owner uuid, after_visibility text, after_principal uuid,
                before_principal uuid, inserted boolean)
     WHERE NOT r.inserted AND s.id = r.id
       AND s.owner_group_id = r.after_owner AND s.visibility = r.after_visibility;
    GET DIAGNOSTICS v_n = ROW_COUNT; v_counts := v_counts || jsonb_build_object('synthesis_jobs', v_n);

    FOR v_name IN SELECT jsonb_object_keys(v_counts) LOOP
        v_n := (v_counts->>v_name)::integer;
        v_total := v_total + v_n;
        IF v_n > 0 THEN
            INSERT INTO security_events (event_type, agent_id, success, details)
            VALUES ('episcience.maint.backfill_reverse', NULL, true,
                    jsonb_build_object('table', v_name, 'rows', v_n,
                                       'target_group', p_manifest->'target_group',
                                       'principal', p_manifest->'principal'));
        END IF;
    END LOOP;
    RETURN v_total;
END $fn$;

ALTER FUNCTION public.episcience_maint_backfill_reverse(jsonb) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_maint_backfill_reverse(jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_maint_backfill_reverse(jsonb) TO episcience_maint_ops;
