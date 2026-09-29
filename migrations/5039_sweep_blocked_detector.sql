SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

-- The lock timeout: nothing here locks a table beyond a catalog entry; kept
-- because the migration lint requires it from 5036 on.

-- 5039_sweep_blocked_detector.sql -- what the narrowing sweep could not
-- narrow, for the maintenance tick's alert.
--
-- WHAT: one SECURITY DEFINER function (the eleventh of the closed set), owned
-- by the kernel maintenance role, `search_path` pinned, EXECUTE revoked from
-- PUBLIC and granted to `episcience_maint_ops` only:
--
--   episcience_maint_unpublishable_public()  RETURNS TABLE (kind, id)
--     every PUBLIC synthesis or sample that is not publishable, by the same
--     two helpers the sweep narrows with. Ids only, never content.
--
-- WHY: the sweep (5037) narrows each row in its own sub-transaction; a row
-- whose narrowing is refused (the known case: a public sample with a public
-- child in another owner's pair) stays public, gets an
-- `episcience.maint.sweep_blocked` audit row on every call, and is otherwise
-- silent. The ratified amendment (2026-09-28) requires an ALERT on those
-- rows. The tick cannot read the audit rows (the `episcience_maint` login
-- holds no table privilege, and reading `security_events` is not in tenancy
-- contract v1), and the sweep returns only the narrowed count, which a
-- return-type change would break for every caller. Called right after the
-- sweep, this lists exactly the rows the sweep just refused to narrow (the
-- sweep tries every public, unpublishable row once per call), so the tick
-- exits with a distinct code while any remains and the unit's failure hook
-- alerts.
--
-- Its answer is a subset of what the sweep already reads and never leaves
-- the maintenance login; it writes nothing.

CREATE FUNCTION public.episcience_maint_unpublishable_public()
RETURNS TABLE (kind text, id uuid)
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path = public, pg_temp AS $fn$
    SELECT 'synthesis'::text, s.id
      FROM public.syntheses s
     WHERE s.visibility = 'public'
       AND NOT public.episcience_synthesis_is_publishable(s.id, s.parent_synthesis_id, s.prereq_synthesis_ids)
    UNION ALL
    SELECT 'sample'::text, s.id
      FROM public.samples s
     WHERE s.visibility = 'public'
       AND NOT public.episcience_sample_is_publishable(s.id, s.parent_sample_id)
     ORDER BY 1, 2
$fn$;

ALTER FUNCTION public.episcience_maint_unpublishable_public() OWNER TO epigraph_maintenance;
REVOKE ALL ON FUNCTION public.episcience_maint_unpublishable_public() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION public.episcience_maint_unpublishable_public() TO episcience_maint_ops;
