-- docs/runbooks/e1f-undo.sql -- compensating SQL for E1f (migrations 5038
-- and 5039), back to the E1e state.
--
-- NOT a migration (the ledger is forward-only). Run by the operator, as the
-- migration owner, in ONE transaction, with the worker and the maintenance
-- timer STOPPED:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/e1f-undo.sql
--
-- It removes 5038's insert-time signature-hash guard (the trigger and its
-- function) and 5039's blocked-row detector, and deletes the two ledger rows,
-- so a later `episcience-migrate run` re-applies both and
-- `episcience-migrate verify` then exits 0. It is the step before
-- docs/runbooks/e1e-undo.sql, which refuses while 5038 or 5039 is recorded.
-- Rows are untouched: a countersignature written without its link while the
-- guard is gone is found by `verify` and repaired by
-- `episcience-migrate backfill-signature-hashes`.
--
-- Refuses unless 5038 or 5039 is recorded, while a later EpiScience migration
-- is recorded, and while any login that is a member of an EpiScience grantee
-- role is connected to this database (the worker, the application logins, the
-- maintenance login).

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version IN (5038, 5039)) THEN
        RAISE EXCEPTION 'neither 5038 nor 5039 is recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version > 5039) THEN
        RAISE EXCEPTION 'a later EpiScience migration is recorded; undo it first';
    END IF;
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

-- ─── 5039 ──────────────────────────────────────────────────────────────────
DROP FUNCTION IF EXISTS public.episcience_maint_unpublishable_public();

-- ─── 5038 ──────────────────────────────────────────────────────────────────
DROP TRIGGER IF EXISTS tenancy_25_signature_hash ON public.countersignatures;
DROP FUNCTION IF EXISTS public.episcience_require_signature_hash();

DELETE FROM episcience_meta._sqlx_migrations WHERE version IN (5038, 5039);
