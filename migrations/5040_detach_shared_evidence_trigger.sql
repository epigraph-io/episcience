SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

-- The lock timeout: DROP TRIGGER takes an ACCESS EXCLUSIVE lock on the
-- kernel's `edges` table, which the running kernel and EpiScience services
-- write. On a busy table the migration gives up after 5 s with nothing
-- applied (re-run it) instead of queueing every edge write behind it.

-- 5040_detach_shared_evidence_trigger.sql -- EpiScience's last object on a
-- kernel table leaves it.
--
-- WHAT: drops the legacy `edges_shared_evidence` AFTER INSERT trigger on
-- `public.edges` and its function `public.create_shared_evidence_factor()`.
-- Both were created by EpiScience's hand-applied `001_initial_schema.sql`
-- (kept in `migrations/legacy/`), never by the kernel; the kernel's own
-- migrations neither create nor reference them.
--
-- WHY: the trigger runs EpiScience code inside every kernel edge insert, on
-- the inserting session, with no pinned `search_path` and no tenancy
-- declaration for the factor rows it writes. That is outside tenancy
-- contract v1 (EpiScience references kernel objects; it does not attach code
-- to them). The consolidated baseline (5032) never created it, so a fresh
-- database has neither object and this migration is a no-op there; on a
-- legacy database it detaches them. Dropping rather than keeping is an
-- operator decision (batch E1, recorded privately).
--
-- EFFECT: the kernel no longer derives `shared_evidence` factors from
-- `analysis --provides_evidence--> claim` edges; existing factor rows are
-- untouched. Undo (compensating, on operator request only):
-- `docs/runbooks/5040-undo.sql`.
--
-- The migration lint admits exactly these two statements, at this version
-- only (`KERNEL_ALLOWLIST` in `crates/episcience-db/tests/migration_lint.rs`).

DROP TRIGGER IF EXISTS edges_shared_evidence ON public.edges;
DROP FUNCTION IF EXISTS public.create_shared_evidence_factor();
