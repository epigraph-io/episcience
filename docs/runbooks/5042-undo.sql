-- docs/runbooks/5042-undo.sql -- compensating SQL for migration 5042: drops
-- the wiki article columns (`seed_theme_id`, `wiki_key`) from `syntheses`,
-- with their two CHECKs and the page index.
--
-- NOT a migration (the ledger is forward-only), and run ONLY on the
-- operator's request. Run by the operator, as the migration owner, in ONE
-- transaction:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/5042-undo.sql
--
-- DATA LOSS: dropping the columns discards every wiki article's page key and
-- seed theme. The syntheses themselves (narratives, citations) stay, but the
-- wiki registry can no longer find them; a later `episcience-migrate run`
-- re-adds the columns EMPTY, so the pages come back only when regenerated.
--
-- It deletes the 5042 ledger row, so a later `episcience-migrate run`
-- re-applies 5042. It is the step before docs/runbooks/5041-undo.sql, which
-- refuses while 5042 is recorded.
--
-- Refuses unless 5042 is recorded, and while a later EpiScience migration is
-- recorded.

SET LOCAL lock_timeout = '5s';

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5042) THEN
        RAISE EXCEPTION '5042 is not recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version > 5042) THEN
        RAISE EXCEPTION 'a later EpiScience migration is recorded; undo it first';
    END IF;
END $guard$;

DROP INDEX public.syntheses_wiki_page_idx;

ALTER TABLE public.syntheses
    DROP CONSTRAINT syntheses_wiki_pair,
    DROP CONSTRAINT syntheses_wiki_key_shape,
    DROP COLUMN wiki_key,
    DROP COLUMN seed_theme_id;

DELETE FROM episcience_meta._sqlx_migrations WHERE version = 5042;
