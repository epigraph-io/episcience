SELECT public.episcience_assert_kernel_contract(1);

-- 5036_row_security.sql -- row security on the 14 EpiScience tables.
--
-- WHAT: ENABLE and FORCE row level security on every EpiScience table; the
-- policies of the tenancy model (the kernel's own shape, bypass arms first);
-- and the grant matrix, starting from nothing:
--
--   table(s)                                         kernel app role   episcience_rw   kernel maintenance
--   the kept-SELECT set (computed from the kernel's
--     entity registry; expected: syntheses)          SELECT            as below        as below
--   syntheses, samples, protocols, blobs,
--     synthesis_clusters, synthesis_embeddings,
--     synthesis_staleness_events, synthesis_provo_edges,
--     synthesis_claim_membership, sample_claims       none              S, I, U, D      S, I, U, D
--   synthesis_jobs, countersignatures                none              S, I            S, I, U, D
--   synthesis_shares, episcience_worker_state        none              none            S, I, U, D
--
-- `episcience_queue` and `episcience_maint_ops` hold no table privilege: they
-- reach the tables only through the maintenance-owned definers (5037).
--
-- WHY NOW: the row guards (5035) already bind every write to the session's
-- principal and groups; this makes READS and the remaining write paths follow
-- the same ownership, so an application login sees exactly the rows its
-- principal may see. The running processes are still privileged, so nothing
-- changes for them until they move to the application logins; the kernel
-- application role loses write access to these tables, as intended.
--
-- Every later EpiScience migration that creates a table repeats the REVOKE:
-- the kernel's default privileges give a new table to its application role.
--
-- One explicit statement per table (migration lint: no loops over names).

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

-- ─── 3. Policies ───────────────────────────────────────────────────────────
-- Bypass arms first in every USING and WITH CHECK. The read disjuncts are in
-- the order of the kernel viewer's predicate, so `/* {VISIBILITY:x} */`
-- splices read the same rows the policy admits.

-- T-PUB: the ownership tables. A row is readable when public or owned by one
-- of the session's groups, writable (insert, and the new row of an update)
-- only into a group the session may write; RESTRICTIVE owner policies make
-- UPDATE and DELETE need write access to the row's CURRENT owner as well, so
-- a public row is readable by everyone and editable by its owners only.
CREATE POLICY syntheses_tenancy ON public.syntheses AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY syntheses_update_owner ON public.syntheses AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY syntheses_delete_owner ON public.syntheses AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY synthesis_clusters_tenancy ON public.synthesis_clusters AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_clusters_update_owner ON public.synthesis_clusters AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_clusters_delete_owner ON public.synthesis_clusters AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY synthesis_embeddings_tenancy ON public.synthesis_embeddings AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_embeddings_update_owner ON public.synthesis_embeddings AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_embeddings_delete_owner ON public.synthesis_embeddings AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY synthesis_staleness_events_tenancy ON public.synthesis_staleness_events AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_staleness_events_update_owner ON public.synthesis_staleness_events AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_staleness_events_delete_owner ON public.synthesis_staleness_events AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY synthesis_provo_edges_tenancy ON public.synthesis_provo_edges AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_provo_edges_update_owner ON public.synthesis_provo_edges AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_provo_edges_delete_owner ON public.synthesis_provo_edges AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY synthesis_claim_membership_tenancy ON public.synthesis_claim_membership AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_claim_membership_update_owner ON public.synthesis_claim_membership AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY synthesis_claim_membership_delete_owner ON public.synthesis_claim_membership AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY samples_tenancy ON public.samples AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY samples_update_owner ON public.samples AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY samples_delete_owner ON public.samples AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY sample_claims_tenancy ON public.sample_claims AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY sample_claims_update_owner ON public.sample_claims AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY sample_claims_delete_owner ON public.sample_claims AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY protocols_tenancy ON public.protocols AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY protocols_update_owner ON public.protocols AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY protocols_delete_owner ON public.protocols AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

CREATE POLICY blobs_tenancy ON public.blobs AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY blobs_update_owner ON public.blobs AS RESTRICTIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY blobs_delete_owner ON public.blobs AS RESTRICTIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));

-- T-PRIV: the job queue. Owner-private (no public arm: a job's payload
-- carries the query and the acting principal). A session inserts only a job
-- that acts as itself in a group it may write; every UPDATE and DELETE is the
-- queue definers' (bypass arms only).
CREATE POLICY synthesis_jobs_read ON public.synthesis_jobs AS PERMISSIVE FOR SELECT TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]));
CREATE POLICY synthesis_jobs_insert ON public.synthesis_jobs AS PERMISSIVE FOR INSERT TO PUBLIC
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR (owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[])
            AND principal_id = (SELECT public.epigraph_principal_id())));
CREATE POLICY synthesis_jobs_bypass_update ON public.synthesis_jobs AS PERMISSIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()));
CREATE POLICY synthesis_jobs_bypass_delete ON public.synthesis_jobs AS PERMISSIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()));

-- T-APPEND: attestations are append-only for a session.
CREATE POLICY countersignatures_read ON public.countersignatures AS PERMISSIVE FOR SELECT TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR visibility = 'public'
        OR owner_group_id = ANY ((SELECT public.epigraph_session_groups())::uuid[]));
CREATE POLICY countersignatures_insert ON public.countersignatures AS PERMISSIVE FOR INSERT TO PUBLIC
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR owner_group_id = ANY ((SELECT public.epigraph_writable_groups())::uuid[]));
CREATE POLICY countersignatures_bypass_update ON public.countersignatures AS PERMISSIVE FOR UPDATE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()));
CREATE POLICY countersignatures_bypass_delete ON public.countersignatures AS PERMISSIVE FOR DELETE TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()));

-- T-CTRL: the two frozen tables (nothing in the application reads or writes
-- them; the share routes answer 410).
CREATE POLICY synthesis_shares_bypass_all ON public.synthesis_shares AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()));
CREATE POLICY episcience_worker_state_bypass_all ON public.episcience_worker_state AS PERMISSIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass()));

-- R: a row that names a kernel claim is readable (and writable) only while the
-- session can read the claim. The subquery runs under the session's own row
-- security on `claims`, so a claim narrowed out of the reader's reach hides the
-- rows that cite it at once, whoever owns them.
CREATE POLICY synthesis_claim_membership_claim_visible ON public.synthesis_claim_membership AS RESTRICTIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = synthesis_claim_membership.claim_id))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = synthesis_claim_membership.claim_id));
CREATE POLICY sample_claims_claim_visible ON public.sample_claims AS RESTRICTIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = sample_claims.claim_id))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = sample_claims.claim_id));
CREATE POLICY countersignatures_claim_visible ON public.countersignatures AS RESTRICTIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = countersignatures.claim_id))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = countersignatures.claim_id));
CREATE POLICY synthesis_provo_edges_claim_visible ON public.synthesis_provo_edges AS RESTRICTIVE FOR ALL TO PUBLIC
    USING (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR synthesis_provo_edges.target_kind <> 'claim'
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = synthesis_provo_edges.target_id))
    WITH CHECK (
        (SELECT public.epigraph_bypass())
        OR (SELECT public.epigraph_definer_bypass())
        OR synthesis_provo_edges.target_kind <> 'claim'
        OR EXISTS (SELECT 1 FROM public.claims c WHERE c.id = synthesis_provo_edges.target_id));
