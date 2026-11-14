-- docs/runbooks/5043-undo.sql -- compensating SQL for migration 5043: drops
-- the external-blob columns (`storage`, `location_uri`, `hash_algorithm`,
-- `is_manifest`, `verify_status`, `last_verified_at`, `last_verified_by`,
-- `last_observed_hash`) from `blobs`, with their CHECKs.
--
-- NOT a migration (the ledger is forward-only), and run ONLY on the
-- operator's request. Run by the operator, as the migration owner, in ONE
-- transaction:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/5043-undo.sql
--
-- REFUSES while any external blob exists. Dropping the columns would turn an
-- external row into a local one whose content_hash the CALLER declared, and
-- the download route reads EPISCIENCE_BLOB_DIR by that hash: whoever
-- registered it could read another tenant's local bytes of that hash. Delete
-- (or export) the external rows first, deliberately; this script never does.
--
-- DATA LOSS: none for local blobs (every local row has the defaults:
-- storage 'local', hash_algorithm 'blake3', is_manifest false, never
-- verified).
--
-- PRECONDITION: run only after the pre-5043 binaries are installed (MCP
-- server, server and worker restarted on them): the 5043+ binaries read and
-- write these columns, so with one still running every blob request fails on
-- the missing columns.
--
-- It deletes the 5043 ledger row, so a later `episcience-migrate run`
-- re-applies 5043. It is the step before docs/runbooks/5042-undo.sql, which
-- refuses while 5043 is recorded.
--
-- Refuses unless 5043 is recorded, and while a later EpiScience migration is
-- recorded.
--
-- FIRST it refuses a session that cannot see every `blobs` row. The table
-- has FORCE ROW LEVEL SECURITY, so a session that is neither a kernel
-- maintenance member (`epigraph_bypass()`) nor a superuser or BYPASSRLS
-- role sees only public rows and its own groups' rows: the external-row
-- guard below would count 0 while another tenant's group external row
-- exists, and the undo would turn that row into a local blob. The same
-- predicate is step 1 of the rollback in docs/deploy.md.

SET LOCAL lock_timeout = '5s';

DO $guard$
BEGIN
    IF NOT (public.epigraph_bypass()
            OR EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                        WHERE r.rolname = current_user AND (r.rolsuper OR r.rolbypassrls))) THEN
        RAISE EXCEPTION 'this session cannot see every blobs row (row security); run as the migration owner in a kernel maintenance, superuser or BYPASSRLS session';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5043) THEN
        RAISE EXCEPTION '5043 is not recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version > 5043) THEN
        RAISE EXCEPTION 'a later EpiScience migration is recorded; undo it first';
    END IF;
    IF EXISTS (SELECT 1 FROM public.blobs WHERE storage <> 'local') THEN
        RAISE EXCEPTION 'external blobs exist; delete them deliberately first (see this script''s header)';
    END IF;
END $guard$;

ALTER TABLE public.blobs
    DROP CONSTRAINT blobs_verify_observation,
    DROP CONSTRAINT blobs_verify_record_whole,
    DROP CONSTRAINT blobs_verify_external_only,
    DROP CONSTRAINT blobs_verify_status_known,
    DROP CONSTRAINT blobs_local_is_blake3,
    DROP CONSTRAINT blobs_location_uri_shape,
    DROP CONSTRAINT blobs_location_by_storage,
    DROP CONSTRAINT blobs_hash_algorithm_known,
    DROP CONSTRAINT blobs_storage_known,
    DROP COLUMN last_observed_hash,
    DROP COLUMN last_verified_by,
    DROP COLUMN last_verified_at,
    DROP COLUMN verify_status,
    DROP COLUMN is_manifest,
    DROP COLUMN hash_algorithm,
    DROP COLUMN location_uri,
    DROP COLUMN storage;

DELETE FROM episcience_meta._sqlx_migrations WHERE version = 5043;
