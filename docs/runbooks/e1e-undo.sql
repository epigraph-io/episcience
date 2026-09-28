-- docs/runbooks/e1e-undo.sql -- compensating SQL for E1e (migrations 5036
-- and 5037), back to the E1d state.
--
-- NOT a migration (the ledger is forward-only). Run by the operator, as the
-- migration owner, in ONE transaction, only while every EpiScience process
-- still runs on the privileged connection (before the worker and the
-- application move to their own logins):
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/e1e-undo.sql
--
-- Use it to go FURTHER back than row security (RUNBOOK section 5): it is the
-- step before docs/runbooks/5035-undo.sql, which refuses while 5036 or 5037 is
-- recorded. To switch row security off and on again only, the lighter pair
-- episcience-rls-undo.sql / episcience-rls-redo.sql is enough.
--
-- It removes, in this order:
--   5037: the seven definers it created (the queue, the worklist, the chain
--         head, the sweep, the member count), and puts back 5035's own bodies
--         of the two publishability helpers (which 5037 re-pointed at the
--         member-count definer, dropped here);
--   5036: the principal guard (12 triggers and its function), the 44
--         policies, FORCE and ENABLE on the 14 tables, and the grant matrix
--         (the pre-5036 privileges come back: the kernel application role
--         S/I/U/D, the EpiScience grantee roles nothing, the kernel
--         maintenance role S/I/U/D);
-- and deletes the 5036 and 5037 ledger rows, so a later `episcience-migrate
-- run` re-applies both, and `episcience-migrate verify` then exits 0.
--
-- It KEEPS, on purpose: `countersignatures.signature_hash` and the
-- countersignature key per recording principal (5037's two schema steps are
-- idempotent, so the re-apply is a no-op for them; the stored link hashes
-- survive; restoring the narrower key could fail on attestations recorded
-- under the wider one); and every row as it is: what the narrowing sweep
-- narrowed stays `group` and `input_narrowed` (a valid E1d state; nothing
-- widens). The sweep's staleness events stay too; 5035-undo removes them if
-- the rollback goes that far.
--
-- Refuses unless 5036 is recorded, while a later migration (E1f's 5038/5039)
-- is recorded (run docs/runbooks/e1f-undo.sql first), and while any login that is a member of an
-- EpiScience grantee role is connected to this database. Works whether or
-- not episcience-rls-undo.sql ran before it.

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5036 AND success) THEN
        RAISE EXCEPTION '5036 is not recorded; nothing to undo';
    END IF;
    -- E1f's migrations sit on top of these: undo them first.
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version > 5037) THEN
        RAISE EXCEPTION 'a later EpiScience migration is recorded; run docs/runbooks/e1f-undo.sql first';
    END IF;
    -- Only while no EpiScience login is connected to this database: the
    -- worker (E1f on) and the application logins (E1g on) depend on the
    -- grants and on row security, and would lose or gain access.
    -- (Superusers are members of every role; the privileged runtime and the
    -- migration owner are not what this looks for.)
    IF EXISTS (SELECT 1 FROM pg_catalog.pg_stat_activity a
                JOIN pg_catalog.pg_roles r ON r.rolname = a.usename
                WHERE a.datname = pg_catalog.current_database()
                  AND a.pid <> pg_catalog.pg_backend_pid()
                  AND NOT r.rolsuper
                  AND (pg_catalog.pg_has_role(a.usename, 'episcience_rw', 'MEMBER')
                       OR pg_catalog.pg_has_role(a.usename, 'episcience_queue', 'MEMBER')
                       OR pg_catalog.pg_has_role(a.usename, 'episcience_maint_ops', 'MEMBER'))) THEN
        RAISE EXCEPTION 'an EpiScience login is connected to this database; stop the units that use one first';
    END IF;
END $guard$;

-- ─── 5037 ──────────────────────────────────────────────────────────────────

-- 5035's own publishability helpers (verbatim), BEFORE the definer they
-- were re-pointed at goes.
CREATE OR REPLACE FUNCTION public.episcience_synthesis_is_publishable(p_id uuid, p_parent uuid, p_prereqs uuid[])
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

CREATE OR REPLACE FUNCTION public.episcience_sample_is_publishable(p_id uuid, p_parent uuid)
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

DROP FUNCTION IF EXISTS public.episcience_members_all_public(text, uuid);
DROP FUNCTION IF EXISTS public.episcience_queue_claim(text);
DROP FUNCTION IF EXISTS public.episcience_queue_finish(uuid, text, text);
DROP FUNCTION IF EXISTS public.episcience_queue_retry(uuid, interval, text);
DROP FUNCTION IF EXISTS public.episcience_owner_worklist(text, integer);
DROP FUNCTION IF EXISTS public.episcience_countersign_chain_head(uuid);
DROP FUNCTION IF EXISTS public.episcience_maint_sweep_narrowed();

-- ─── 5036 ──────────────────────────────────────────────────────────────────

DROP TRIGGER IF EXISTS tenancy_05_principal ON public.syntheses;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.synthesis_clusters;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.synthesis_embeddings;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.synthesis_staleness_events;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.synthesis_provo_edges;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.synthesis_claim_membership;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.synthesis_jobs;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.samples;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.sample_claims;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.protocols;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.blobs;
DROP TRIGGER IF EXISTS tenancy_05_principal ON public.countersignatures;
DROP FUNCTION IF EXISTS public.episcience_require_principal();

DROP POLICY IF EXISTS syntheses_tenancy ON public.syntheses;
DROP POLICY IF EXISTS syntheses_update_owner ON public.syntheses;
DROP POLICY IF EXISTS syntheses_delete_owner ON public.syntheses;
DROP POLICY IF EXISTS synthesis_clusters_tenancy ON public.synthesis_clusters;
DROP POLICY IF EXISTS synthesis_clusters_update_owner ON public.synthesis_clusters;
DROP POLICY IF EXISTS synthesis_clusters_delete_owner ON public.synthesis_clusters;
DROP POLICY IF EXISTS synthesis_embeddings_tenancy ON public.synthesis_embeddings;
DROP POLICY IF EXISTS synthesis_embeddings_update_owner ON public.synthesis_embeddings;
DROP POLICY IF EXISTS synthesis_embeddings_delete_owner ON public.synthesis_embeddings;
DROP POLICY IF EXISTS synthesis_staleness_events_tenancy ON public.synthesis_staleness_events;
DROP POLICY IF EXISTS synthesis_staleness_events_update_owner ON public.synthesis_staleness_events;
DROP POLICY IF EXISTS synthesis_staleness_events_delete_owner ON public.synthesis_staleness_events;
DROP POLICY IF EXISTS synthesis_provo_edges_tenancy ON public.synthesis_provo_edges;
DROP POLICY IF EXISTS synthesis_provo_edges_update_owner ON public.synthesis_provo_edges;
DROP POLICY IF EXISTS synthesis_provo_edges_delete_owner ON public.synthesis_provo_edges;
DROP POLICY IF EXISTS synthesis_claim_membership_tenancy ON public.synthesis_claim_membership;
DROP POLICY IF EXISTS synthesis_claim_membership_update_owner ON public.synthesis_claim_membership;
DROP POLICY IF EXISTS synthesis_claim_membership_delete_owner ON public.synthesis_claim_membership;
DROP POLICY IF EXISTS samples_tenancy ON public.samples;
DROP POLICY IF EXISTS samples_update_owner ON public.samples;
DROP POLICY IF EXISTS samples_delete_owner ON public.samples;
DROP POLICY IF EXISTS sample_claims_tenancy ON public.sample_claims;
DROP POLICY IF EXISTS sample_claims_update_owner ON public.sample_claims;
DROP POLICY IF EXISTS sample_claims_delete_owner ON public.sample_claims;
DROP POLICY IF EXISTS protocols_tenancy ON public.protocols;
DROP POLICY IF EXISTS protocols_update_owner ON public.protocols;
DROP POLICY IF EXISTS protocols_delete_owner ON public.protocols;
DROP POLICY IF EXISTS blobs_tenancy ON public.blobs;
DROP POLICY IF EXISTS blobs_update_owner ON public.blobs;
DROP POLICY IF EXISTS blobs_delete_owner ON public.blobs;
DROP POLICY IF EXISTS synthesis_jobs_read ON public.synthesis_jobs;
DROP POLICY IF EXISTS synthesis_jobs_insert ON public.synthesis_jobs;
DROP POLICY IF EXISTS synthesis_jobs_bypass_update ON public.synthesis_jobs;
DROP POLICY IF EXISTS synthesis_jobs_bypass_delete ON public.synthesis_jobs;
DROP POLICY IF EXISTS countersignatures_read ON public.countersignatures;
DROP POLICY IF EXISTS countersignatures_insert ON public.countersignatures;
DROP POLICY IF EXISTS countersignatures_bypass_update ON public.countersignatures;
DROP POLICY IF EXISTS countersignatures_bypass_delete ON public.countersignatures;
DROP POLICY IF EXISTS synthesis_shares_bypass_all ON public.synthesis_shares;
DROP POLICY IF EXISTS episcience_worker_state_bypass_all ON public.episcience_worker_state;
DROP POLICY IF EXISTS synthesis_claim_membership_claim_visible ON public.synthesis_claim_membership;
DROP POLICY IF EXISTS sample_claims_claim_visible ON public.sample_claims;
DROP POLICY IF EXISTS countersignatures_claim_visible ON public.countersignatures;
DROP POLICY IF EXISTS synthesis_provo_edges_claim_visible ON public.synthesis_provo_edges;

ALTER TABLE public.syntheses NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.syntheses DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_clusters NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_clusters DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_embeddings NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_embeddings DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_staleness_events NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_staleness_events DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_provo_edges NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_provo_edges DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_claim_membership NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_claim_membership DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_jobs NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_jobs DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_shares NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_shares DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.samples NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.samples DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.sample_claims NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.sample_claims DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.protocols NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.protocols DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.blobs NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.blobs DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.countersignatures NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.countersignatures DISABLE ROW LEVEL SECURITY;
ALTER TABLE public.episcience_worker_state NO FORCE ROW LEVEL SECURITY;
ALTER TABLE public.episcience_worker_state DISABLE ROW LEVEL SECURITY;

REVOKE ALL ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    FROM episcience_rw, episcience_queue, episcience_maint_ops;
GRANT SELECT, INSERT, UPDATE, DELETE ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    TO epigraph_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    TO epigraph_maintenance;

-- ─── The ledger: 5036 and 5037 are no longer applied ───────────────────────
DELETE FROM episcience_meta._sqlx_migrations WHERE version IN (5036, 5037);
