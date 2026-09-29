-- docs/runbooks/episcience-rls-undo.sql -- compensating SQL for the row-security
-- migration (5036).
--
-- NOT a migration (the ledger is forward-only). Run by the operator, as the
-- migration owner, in ONE transaction, only to back out row security while
-- every EpiScience process still runs on the privileged connection (before
-- the worker and the application move to their own logins):
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/episcience-rls-undo.sql
--
-- It turns row security OFF on the 14 tables (NO FORCE, then DISABLE) and puts
-- back the table privileges they had before 5036: the kernel application role
-- S/I/U/D (what the kernel's default privileges gave it), the EpiScience
-- grantee roles nothing, the kernel maintenance role S/I/U/D. The policies,
-- the 5037 definers and the ledger rows stay; `episcience-rls-redo.sql`
-- restores 5036's state exactly. While this is in effect
-- `episcience-migrate verify` REFUSES (it names every table without row
-- security and every grant outside the matrix): that is the intended signal.
--
-- Refuses unless 5036 is recorded, and while any login that is a member of an
-- EpiScience grantee role is connected to this database. No row changes.

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5036 AND success) THEN
        RAISE EXCEPTION '5036 is not recorded; nothing to undo';
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
