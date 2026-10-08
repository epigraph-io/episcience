-- docs/runbooks/5041-undo.sql -- compensating SQL for migration 5041: narrows
-- the `syntheses_skill_name_known` CHECK back to the five skills before
-- `wiki_article`.
--
-- NOT a migration (the ledger is forward-only), and run ONLY on the
-- operator's request. Run by the operator, as the migration owner, in ONE
-- transaction:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/5041-undo.sql
--
-- It re-adds the CHECK exactly as 5032 created it and deletes the 5041
-- ledger row, so a later `episcience-migrate run` widens it again. It is the
-- step before docs/runbooks/5040-undo.sql, which refuses while 5041 is
-- recorded.
--
-- Refuses unless 5041 is recorded, while a later EpiScience migration is
-- recorded, and while any `wiki_article` synthesis exists (the narrowed CHECK
-- would reject it; delete or keep those rows by an operator decision first).

SET LOCAL lock_timeout = '5s';

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5041) THEN
        RAISE EXCEPTION '5041 is not recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version > 5041) THEN
        RAISE EXCEPTION 'a later EpiScience migration is recorded; undo it first';
    END IF;
    IF EXISTS (SELECT 1 FROM public.syntheses WHERE skill_name = 'wiki_article') THEN
        RAISE EXCEPTION 'wiki_article syntheses exist; the narrowed CHECK would reject them; nothing changed';
    END IF;
END $guard$;

ALTER TABLE public.syntheses
    DROP CONSTRAINT syntheses_skill_name_known,
    ADD CONSTRAINT syntheses_skill_name_known
        CHECK (skill_name = ANY (ARRAY['baseline', 'lab_notebook', 'literature', 'code_review', 'registry_diff']));

DELETE FROM episcience_meta._sqlx_migrations WHERE version = 5041;
