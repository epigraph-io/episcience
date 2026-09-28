SELECT public.episcience_assert_kernel_contract(1);

-- 5037_queue_and_maintenance_definers.sql -- the rest of the closed definer set.
--
-- WHAT: seven SECURITY DEFINER functions, each owned by the kernel maintenance
-- role, with `search_path` pinned, EXECUTE revoked from PUBLIC and granted to
-- exactly one EpiScience NOLOGIN role:
--
--   episcience_members_all_public(kind, id)   episcience_rw         are ALL of a row's cited claims public
--   episcience_queue_claim(worker)            episcience_queue      take the next runnable job
--   episcience_queue_finish(job, state, err)  episcience_queue      running -> complete | failed
--   episcience_queue_retry(job, delay, err)   episcience_queue      running -> queued, later
--   episcience_owner_worklist(kind, limit)    episcience_queue      (synthesis, acting principal) pairs
--   episcience_countersign_chain_head(claim)  episcience_rw         the claim's latest signature
--   episcience_maint_sweep_narrowed()         episcience_maint_ops  narrow what stopped being publishable
--
-- It also:
--   * re-points the member half of 5035's two publishability helpers at
--     episcience_members_all_public.
--
-- With the two backfill definers (5034) and the propagation (5035) this is the
-- whole set; `episcience-migrate verify` refuses a database with any other.
--
-- WHY DEFINERS: row security (5036) gives an application session no UPDATE or
-- DELETE on the job queue (its policies admit only the bypass arms), and no
-- session can see another owner's rows. Each function below is one narrow,
-- audited act across owners that the tenancy model needs: the worker moves a
-- job through its states and learns which syntheses need work (ids and the
-- acting principal only, never content); a countersignature chains on the
-- claim's latest signature whoever wrote it; the maintenance timer narrows a
-- public synthesis whose inputs stopped being public. Everything else stays on
-- the session's own row security.
--
-- One explicit statement per function (migration lint).

-- ─── Publishability across rows the session cannot see (EXECUTE: episcience_rw)

-- 5035's helpers judged "every member claim public" on the SESSION's rows:
-- `NOT EXISTS (member whose claim is hidden or not public)`. That relied on
-- the membership row staying visible while only its claim was hidden. 5036's
-- RESTRICTIVE `<t>_claim_visible` policies hide the MEMBERSHIP row itself once
-- the session cannot read its claim, so on an application session the
-- NOT EXISTS became vacuously true: an owner could widen, and a completing
-- job keep public, a synthesis or sample citing a claim narrowed out of the
-- owner's reach.
--
-- This definer counts over ALL of the row's membership (or sample_claims)
-- rows and all claims. It answers only for a row the CALLER may read (a
-- privileged session, a public row, or a row owned by one of the caller's
-- groups, read from the caller's own session settings, never from the
-- definer's own maintenance bypass); for any other id, existing or not, it
-- answers true, exactly as for a row with no members, so it says nothing
-- about a row the caller cannot read. Every guard that consults it runs on a
-- row the session can see (row security filters an UPDATE's targets) or on a
-- row being inserted (no members yet).
CREATE FUNCTION public.episcience_members_all_public(p_kind text, p_id uuid)
RETURNS boolean
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_vis   text;
    v_owner uuid;
BEGIN
    IF p_kind = 'synthesis' THEN
        SELECT s.visibility, s.owner_group_id INTO v_vis, v_owner FROM syntheses s WHERE s.id = p_id;
    ELSIF p_kind = 'sample' THEN
        SELECT s.visibility, s.owner_group_id INTO v_vis, v_owner FROM samples s WHERE s.id = p_id;
    ELSE
        RAISE EXCEPTION 'episcience publishability: unknown kind %', p_kind USING ERRCODE = '22023';
    END IF;
    IF NOT FOUND
       OR NOT ((SELECT public.epigraph_bypass())
               OR v_vis = 'public'
               OR v_owner = ANY ((SELECT public.epigraph_session_groups())::uuid[])) THEN
        RETURN true;
    END IF;
    IF p_kind = 'synthesis' THEN
        RETURN NOT EXISTS (SELECT 1 FROM synthesis_claim_membership m
                             LEFT JOIN claims c ON c.id = m.claim_id
                            WHERE m.synthesis_id = p_id
                              AND (c.id IS NULL OR c.visibility::text <> 'public'));
    END IF;
    RETURN NOT EXISTS (SELECT 1 FROM sample_claims sc
                         LEFT JOIN claims c ON c.id = sc.claim_id
                        WHERE sc.sample_id = p_id
                          AND (c.id IS NULL OR c.visibility::text <> 'public'));
END $fn$;

ALTER FUNCTION public.episcience_members_all_public(text, uuid) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_members_all_public(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_members_all_public(text, uuid) TO episcience_rw;

-- The two INVOKER helpers (5035) keep their signatures, their callers (the
-- widening guard, the publish rule, the require guard, the sweep) and their
-- parent and prerequisite arms, which stay on the SESSION's rows (a hidden
-- parent or prerequisite is non-public by construction: a public row is
-- visible to every session). Only the member arm moves to the definer.
CREATE OR REPLACE FUNCTION public.episcience_synthesis_is_publishable(p_id uuid, p_parent uuid, p_prereqs uuid[])
RETURNS boolean
LANGUAGE sql STABLE SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
    SELECT public.episcience_members_all_public('synthesis', p_id)
       AND (p_parent IS NULL
            OR EXISTS (SELECT 1 FROM syntheses p WHERE p.id = p_parent AND p.visibility = 'public'))
       AND NOT EXISTS (SELECT 1 FROM unnest(coalesce(p_prereqs, ARRAY[]::uuid[])) x(id)
                         LEFT JOIN syntheses p ON p.id = x.id
                        WHERE p.id IS NULL OR p.visibility <> 'public')
$fn$;

CREATE OR REPLACE FUNCTION public.episcience_sample_is_publishable(p_id uuid, p_parent uuid)
RETURNS boolean
LANGUAGE sql STABLE SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
    SELECT public.episcience_members_all_public('sample', p_id)
       AND (p_parent IS NULL
            OR EXISTS (SELECT 1 FROM samples p WHERE p.id = p_parent AND p.visibility = 'public'))
$fn$;

-- ─── The job queue (EXECUTE: episcience_queue) ─────────────────────────────

-- Take the earliest runnable job (`queued` or `retry`, due now), skipping rows
-- another worker holds, and mark it `running` with one more attempt. Returns
-- no row when nothing is due. The worker's name is required and goes to the
-- server log (the queue table has no claimant column).
CREATE FUNCTION public.episcience_queue_claim(p_worker text)
RETURNS TABLE (job_id uuid, synthesis_id uuid, principal_id uuid, job_type text, payload jsonb,
               attempts integer, created_at timestamp with time zone)
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
#variable_conflict use_column
BEGIN
    IF p_worker IS NULL OR btrim(p_worker) = '' THEN
        RAISE EXCEPTION 'episcience queue: a worker name is required' USING ERRCODE = '22004';
    END IF;
    RETURN QUERY
    WITH next_job AS (
        SELECT j.id
          FROM synthesis_jobs j
         WHERE j.state IN ('queued', 'retry')
           AND j.scheduled_at <= now()
         ORDER BY j.scheduled_at, j.id
         LIMIT 1
         FOR UPDATE SKIP LOCKED
    )
    UPDATE synthesis_jobs j
       SET state = 'running',
           started_at = now(),
           attempts = j.attempts + 1,
           updated_at = now()
      FROM next_job n
     WHERE j.id = n.id
    RETURNING j.id, j.id, j.principal_id, j.job_type, j.payload, j.attempts, j.created_at;
    IF FOUND THEN
        RAISE LOG 'episcience queue: worker % claimed a job', p_worker;
    END IF;
END $fn$;

ALTER FUNCTION public.episcience_queue_claim(text) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_queue_claim(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_queue_claim(text) TO episcience_queue;

-- A running job ends `complete` or `failed`; anything else is refused (a job
-- that is not running, an unknown state).
CREATE FUNCTION public.episcience_queue_finish(p_job uuid, p_state text, p_error text)
RETURNS void
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_n integer;
BEGIN
    IF p_job IS NULL OR p_state IS NULL OR p_state NOT IN ('complete', 'failed') THEN
        RAISE EXCEPTION 'episcience queue: finish takes a job and the state complete or failed'
            USING ERRCODE = '22023';
    END IF;
    UPDATE synthesis_jobs j
       SET state = p_state,
           completed_at = now(),
           last_error = p_error,
           updated_at = now()
     WHERE j.id = p_job
       AND j.state = 'running';
    GET DIAGNOSTICS v_n = ROW_COUNT;
    IF v_n <> 1 THEN
        RAISE EXCEPTION 'episcience queue: job % is not running', p_job USING ERRCODE = '55000';
    END IF;
END $fn$;

ALTER FUNCTION public.episcience_queue_finish(uuid, text, text) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_queue_finish(uuid, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_queue_finish(uuid, text, text) TO episcience_queue;

-- A running job with attempts left goes back to `queued`, due after the
-- delay. It never inserts: a synthesis keeps exactly one job row. A job that
-- has used its attempts is refused (the worker finishes it as failed).
CREATE FUNCTION public.episcience_queue_retry(p_job uuid, p_delay interval, p_error text)
RETURNS void
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_n integer;
BEGIN
    IF p_job IS NULL OR p_delay IS NULL OR p_delay < interval '0' THEN
        RAISE EXCEPTION 'episcience queue: retry takes a job and a non-negative delay'
            USING ERRCODE = '22023';
    END IF;
    UPDATE synthesis_jobs j
       SET state = 'queued',
           scheduled_at = now() + p_delay,
           started_at = NULL,
           last_error = p_error,
           updated_at = now()
     WHERE j.id = p_job
       AND j.state = 'running'
       AND j.attempts < j.max_attempts;
    GET DIAGNOSTICS v_n = ROW_COUNT;
    IF v_n <> 1 THEN
        IF EXISTS (SELECT 1 FROM synthesis_jobs j WHERE j.id = p_job AND j.state = 'running') THEN
            RAISE EXCEPTION 'episcience queue: job % has used all its attempts', p_job
                USING ERRCODE = '55000',
                      HINT = 'finish it as failed';
        END IF;
        RAISE EXCEPTION 'episcience queue: job % is not running', p_job USING ERRCODE = '55000';
    END IF;
END $fn$;

ALTER FUNCTION public.episcience_queue_retry(uuid, interval, text) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_queue_retry(uuid, interval, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_queue_retry(uuid, interval, text) TO episcience_queue;

-- The work the worker does outside the queue, as (synthesis, principal) pairs:
-- the principal is the synthesis' job principal, and the worker then acts
-- stamped as that principal. Ids only, never content.
--   stage6_pending : complete, PUBLIC, with an unwritten outbox row that is not
--                    deferred and is under the retry cap (10 attempts);
--   staleness_check: complete, not stale, not checked in the last 15 minutes.
-- Only syntheses whose job principal can still WRITE the owner group (a live
-- `admin` or `writer` membership, the kernel's writable rule): the worker
-- writes nothing for any other (it re-checks authority before each item), so
-- such an item could never advance its own position and, once `p_limit` of
-- them existed, would starve every other owner's work.
CREATE FUNCTION public.episcience_owner_worklist(p_kind text, p_limit integer)
RETURNS TABLE (synthesis_id uuid, principal_id uuid)
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
#variable_conflict use_column
BEGIN
    IF p_limit IS NULL OR p_limit < 1 OR p_limit > 1000 THEN
        RAISE EXCEPTION 'episcience worklist: the limit must be between 1 and 1000' USING ERRCODE = '22023';
    END IF;
    IF p_kind = 'stage6_pending' THEN
        RETURN QUERY
        SELECT s.id, j.principal_id
          FROM syntheses s
          JOIN synthesis_jobs j ON j.id = s.id
         WHERE s.status = 'complete'
           AND s.visibility = 'public'
           AND EXISTS (SELECT 1 FROM synthesis_provo_edges pe
                        WHERE pe.synthesis_id = s.id
                          AND pe.written_at IS NULL
                          AND pe.deferred_reason IS NULL
                          AND pe.attempt_count < 10)
           AND EXISTS (SELECT 1 FROM group_memberships gm
                        WHERE gm.group_id = s.owner_group_id
                          AND gm.agent_id = j.principal_id
                          AND gm.revoked_at IS NULL
                          AND gm.role::text IN ('admin', 'writer'))
         ORDER BY s.completed_at, s.id
         LIMIT p_limit;
    ELSIF p_kind = 'staleness_check' THEN
        RETURN QUERY
        SELECT s.id, j.principal_id
          FROM syntheses s
          JOIN synthesis_jobs j ON j.id = s.id
         WHERE s.status = 'complete'
           AND s.stale_since IS NULL
           AND (s.staleness_checked_at IS NULL
                OR s.staleness_checked_at < now() - interval '15 minutes')
           AND EXISTS (SELECT 1 FROM group_memberships gm
                        WHERE gm.group_id = s.owner_group_id
                          AND gm.agent_id = j.principal_id
                          AND gm.revoked_at IS NULL
                          AND gm.role::text IN ('admin', 'writer'))
         ORDER BY s.staleness_checked_at NULLS FIRST, s.id
         LIMIT p_limit;
    ELSE
        RAISE EXCEPTION 'episcience worklist: unknown kind %', p_kind USING ERRCODE = '22023';
    END IF;
END $fn$;

ALTER FUNCTION public.episcience_owner_worklist(text, integer) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_owner_worklist(text, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_owner_worklist(text, integer) TO episcience_queue;

-- ─── The countersignature chain (EXECUTE: episcience_rw) ────────────────────

-- The claim's latest signature, whoever wrote it (the chain runs across
-- writers, whose rows the caller may not see), or NULL when the claim has none
-- yet. Takes the per-claim transaction lock first, so the caller's append in
-- the same transaction cannot race another. Refused (42501, never a NULL that
-- would fork the chain) unless the caller may read the claim: a privileged
-- session, a public claim, or a claim owned by one of the caller's groups
-- (read from the caller's own session settings).
CREATE FUNCTION public.episcience_countersign_chain_head(p_claim uuid)
RETURNS bytea
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_vis   text;
    v_owner uuid;
    v_head  bytea;
BEGIN
    IF p_claim IS NULL THEN
        RAISE EXCEPTION 'episcience chain head: a claim is required' USING ERRCODE = '22004';
    END IF;
    SELECT c.visibility::text, c.owner_group_id INTO v_vis, v_owner FROM claims c WHERE c.id = p_claim;
    IF NOT FOUND
       OR NOT ((SELECT public.epigraph_bypass())
               OR v_vis = 'public'
               OR v_owner = ANY ((SELECT public.epigraph_session_groups())::uuid[])) THEN
        RAISE EXCEPTION 'claim % is not visible', p_claim USING ERRCODE = '42501';
    END IF;
    PERFORM pg_advisory_xact_lock(hashtext(p_claim::text));
    SELECT cs.signature INTO v_head
      FROM countersignatures cs
     WHERE cs.claim_id = p_claim
     ORDER BY cs.created_at DESC, cs.id DESC
     LIMIT 1;
    RETURN v_head;
END $fn$;

ALTER FUNCTION public.episcience_countersign_chain_head(uuid) OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_countersign_chain_head(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_countersign_chain_head(uuid) TO episcience_rw;

-- ─── The narrowing sweep (EXECUTE: episcience_maint_ops) ────────────────────

-- Narrow-only, idempotent: every PUBLIC synthesis that is no longer
-- publishable (a member claim, its parent or a prerequisite stopped being
-- public, out of band) becomes `group` (its children follow through the
-- propagation), is marked stale `input_narrowed` unless already stale, gets a
-- staleness event, and one audit row; every PUBLIC sample that is no longer
-- publishable (an attached claim or its parent stopped being public) becomes
-- `group` (its claims' rows, blobs and child samples follow) with one audit
-- row. Repeated until nothing changes (a narrowed parent makes its public
-- children non-publishable in turn). Never widens, never re-owns. Returns the
-- number of rows narrowed.
--
-- The staleness event names only the non-public member claims the synthesis'
-- OWNER GROUP owns: a claim narrowed to another group is hidden from the
-- synthesis' readers (its membership row too), and naming it here would hand
-- them back exactly the id row security just hid.
CREATE FUNCTION public.episcience_maint_sweep_narrowed()
RETURNS integer
LANGUAGE plpgsql VOLATILE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
DECLARE
    v_total integer := 0;
    v_n     integer;
    r       record;
BEGIN
    LOOP
        v_n := 0;
        FOR r IN
            UPDATE syntheses s
               SET visibility = 'group',
                   stale_since = coalesce(s.stale_since, now()),
                   stale_reason = coalesce(s.stale_reason, 'input_narrowed')
             WHERE s.visibility = 'public'
               AND NOT public.episcience_synthesis_is_publishable(s.id, s.parent_synthesis_id, s.prereq_synthesis_ids)
            RETURNING s.id, s.owner_group_id
        LOOP
            INSERT INTO synthesis_staleness_events (id, synthesis_id, trigger, affected_claim_ids, detail)
            VALUES (gen_random_uuid(), r.id, 'input_narrowed',
                    ARRAY(SELECT m.claim_id
                            FROM synthesis_claim_membership m
                            JOIN claims c ON c.id = m.claim_id
                           WHERE m.synthesis_id = r.id
                             AND c.visibility::text <> 'public'
                             AND c.owner_group_id = r.owner_group_id
                           ORDER BY m.claim_id),
                    jsonb_build_object('reason', 'a member claim, the parent or a prerequisite is no longer public'));
            INSERT INTO security_events (event_type, agent_id, success, details)
            VALUES ('episcience.maint.sweep_narrowed', NULL, true,
                    jsonb_build_object('synthesis_id', r.id, 'owner_group_id', r.owner_group_id));
            v_n := v_n + 1;
        END LOOP;
        FOR r IN
            UPDATE samples s
               SET visibility = 'group'
             WHERE s.visibility = 'public'
               AND NOT public.episcience_sample_is_publishable(s.id, s.parent_sample_id)
            RETURNING s.id, s.owner_group_id
        LOOP
            INSERT INTO security_events (event_type, agent_id, success, details)
            VALUES ('episcience.maint.sweep_narrowed', NULL, true,
                    jsonb_build_object('sample_id', r.id, 'owner_group_id', r.owner_group_id));
            v_n := v_n + 1;
        END LOOP;
        v_total := v_total + v_n;
        EXIT WHEN v_n = 0;
    END LOOP;
    RETURN v_total;
END $fn$;

ALTER FUNCTION public.episcience_maint_sweep_narrowed() OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_maint_sweep_narrowed() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_maint_sweep_narrowed() TO episcience_maint_ops;
