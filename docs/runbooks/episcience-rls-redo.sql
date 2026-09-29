-- docs/runbooks/episcience-rls-redo.sql -- re-applies row security after
-- docs/runbooks/episcience-rls-undo.sql.
--
-- NOT a migration. Run by the operator, as the migration owner, in ONE
-- transaction:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/episcience-rls-redo.sql
--
-- It is sections 1 and 2 of migrations/5036_row_security.sql verbatim (the
-- privileges from nothing, then ENABLE + FORCE); the policies were never
-- dropped. Afterwards `episcience-migrate verify` must exit 0 again. A test
-- keeps this file equal to the migration's text.
--
-- Refuses unless 5036 is recorded and every 5036 policy is still present.

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5036 AND success) THEN
        RAISE EXCEPTION '5036 is not recorded; run episcience-migrate instead';
    END IF;
    IF (SELECT count(*) FROM pg_catalog.pg_policy p
          JOIN pg_catalog.pg_class c ON c.oid = p.polrelid
         WHERE c.relnamespace = 'public'::pg_catalog.regnamespace
           AND c.relname IN ('syntheses', 'synthesis_clusters', 'synthesis_embeddings', 'synthesis_staleness_events', 'synthesis_provo_edges', 'synthesis_claim_membership', 'synthesis_jobs', 'synthesis_shares', 'samples', 'sample_claims', 'protocols', 'blobs', 'countersignatures', 'episcience_worker_state')) <> 44 THEN
        RAISE EXCEPTION 'the 5036 policies are not all present; this script restores grants and flags only';
    END IF;
END $guard$;

-- ─── 1. Privileges ──────────────────────────────────────────────────────────

-- Start from nothing: the kernel's default privileges gave every table a
-- migration creates to the kernel application role (and PUBLIC may hold
-- something on a hand-built database). The three EpiScience grantee roles are
-- cleared too, so the grants below are the whole of their table privileges.

REVOKE ALL ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    FROM PUBLIC;
REVOKE ALL ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    FROM epigraph_app;
REVOKE ALL ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    FROM episcience_rw, episcience_queue, episcience_maint_ops;

-- The kept-SELECT set: an EpiScience table the kernel's entity registry names
-- stays READABLE by the kernel application role (under the policies below), so
-- the kernel's edge-reference check can see a synthesis its caller may see.
-- Computed from the registry, one explicit statement per table (no dynamic
-- SQL): expected to be `syntheses` alone.
DO $kept$
BEGIN
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'syntheses') THEN
        GRANT SELECT ON public.syntheses TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_clusters') THEN
        GRANT SELECT ON public.synthesis_clusters TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_embeddings') THEN
        GRANT SELECT ON public.synthesis_embeddings TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_staleness_events') THEN
        GRANT SELECT ON public.synthesis_staleness_events TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_provo_edges') THEN
        GRANT SELECT ON public.synthesis_provo_edges TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_claim_membership') THEN
        GRANT SELECT ON public.synthesis_claim_membership TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_jobs') THEN
        GRANT SELECT ON public.synthesis_jobs TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'synthesis_shares') THEN
        GRANT SELECT ON public.synthesis_shares TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'samples') THEN
        GRANT SELECT ON public.samples TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'sample_claims') THEN
        GRANT SELECT ON public.sample_claims TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'protocols') THEN
        GRANT SELECT ON public.protocols TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'blobs') THEN
        GRANT SELECT ON public.blobs TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'countersignatures') THEN
        GRANT SELECT ON public.countersignatures TO epigraph_app;
    END IF;
    IF EXISTS (SELECT 1 FROM public.entity_types e
                WHERE e.schema_name = 'public' AND e.table_name = 'episcience_worker_state') THEN
        GRANT SELECT ON public.episcience_worker_state TO epigraph_app;
    END IF;
END $kept$;

-- The EpiScience application roles (the logins are members): full access to
-- the ownership tables, read and append on the job queue and the
-- attestations (their updates go through maintenance-owned definers), nothing
-- on the two frozen tables.
GRANT SELECT, INSERT, UPDATE, DELETE ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.samples, public.sample_claims, public.protocols,
    public.blobs
    TO episcience_rw;
GRANT SELECT, INSERT ON public.synthesis_jobs, public.countersignatures TO episcience_rw;

-- The kernel maintenance role owns every EpiScience definer (restated from the
-- expand step; idempotent).
GRANT SELECT, INSERT, UPDATE, DELETE ON
    public.syntheses, public.synthesis_clusters, public.synthesis_embeddings,
    public.synthesis_staleness_events, public.synthesis_provo_edges,
    public.synthesis_claim_membership, public.synthesis_jobs, public.synthesis_shares,
    public.samples, public.sample_claims, public.protocols, public.blobs, public.countersignatures,
    public.episcience_worker_state
    TO epigraph_maintenance;

-- ─── 2. Row security on, forced ──────────────────────────────────────────────

ALTER TABLE public.syntheses ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.syntheses FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_clusters ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_clusters FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_embeddings ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_embeddings FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_staleness_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_staleness_events FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_provo_edges ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_provo_edges FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_claim_membership ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_claim_membership FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_jobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_jobs FORCE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_shares ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.synthesis_shares FORCE ROW LEVEL SECURITY;
ALTER TABLE public.samples ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.samples FORCE ROW LEVEL SECURITY;
ALTER TABLE public.sample_claims ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.sample_claims FORCE ROW LEVEL SECURITY;
ALTER TABLE public.protocols ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.protocols FORCE ROW LEVEL SECURITY;
ALTER TABLE public.blobs ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.blobs FORCE ROW LEVEL SECURITY;
ALTER TABLE public.countersignatures ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.countersignatures FORCE ROW LEVEL SECURITY;
ALTER TABLE public.episcience_worker_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE public.episcience_worker_state FORCE ROW LEVEL SECURITY;
