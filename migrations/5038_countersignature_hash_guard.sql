SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

-- The lock timeout: CREATE TRIGGER on countersignatures takes a SHARE ROW
-- EXCLUSIVE lock while the runtime keeps serving; a long transaction holding
-- the table makes this migration give up after 5 s (nothing of it applied;
-- re-run it) instead of queueing every later query.

-- 5038_countersignature_hash_guard.sql -- a countersignature written by a
-- non-privileged session must carry its chain link.
--
-- WHAT: one row guard, SECURITY INVOKER (R5b), on countersignatures:
--
--   tenancy_25_signature_hash  BEFORE INSERT, FOR EACH ROW
--     episcience_require_signature_hash(): a NULL `signature_hash` from a
--     non-privileged session is refused (23502).
--
-- WHY: 5037 gave each countersignature the hash of its own signature, the
-- link the next attestation of the same claim chains on (the chain head
-- returns that hash, never the signature). The writer fills it; `verify` and
-- `episcience-migrate backfill-signature-hashes` repair rows written without
-- it by an older binary. From the first application login on, a row WITHOUT
-- the link would let the next attestation chain on nothing the reader can
-- check (the chain head falls back to a readable older signature, or refuses),
-- so an application session may never write one. The `episcience_worker`
-- login is a member of `episcience_rw`, which holds INSERT on
-- countersignatures: that deploy is the first time a non-privileged login can
-- write this table, so the guard ships before it (brief amendment
-- 2026-09-28).
--
-- A PRIVILEGED session is exempt: the migration owner's backfill and the
-- privileged runtime, which run before the switch, are the repair path, and
-- `verify` refuses a database with any row left without a link.
--
-- The trigger name orders it after the author binding (15) and the claim
-- guard (20): it judges the row the earlier guards completed.

CREATE FUNCTION public.episcience_require_signature_hash()
RETURNS trigger
LANGUAGE plpgsql SECURITY INVOKER
SET search_path = public, pg_temp AS $fn$
BEGIN
    IF NEW.signature_hash IS NULL AND NOT public.episcience_session_is_privileged() THEN
        RAISE EXCEPTION 'a countersignature must carry the hash of its signature'
            USING ERRCODE = '23502',
                  HINT = 'store the hash of the signature in signature_hash: the next attestation of the claim chains on it';
    END IF;
    RETURN NEW;
END $fn$;

CREATE TRIGGER tenancy_25_signature_hash BEFORE INSERT ON public.countersignatures
    FOR EACH ROW EXECUTE FUNCTION public.episcience_require_signature_hash();
