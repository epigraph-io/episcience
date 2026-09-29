-- docs/runbooks/e1c-rollback-vocabulary.sql -- make the data decodable by the
-- previous (E1c) binary before it is started again.
--
-- NOT a migration. Run by the operator, as the migration owner, in ONE
-- transaction, as the LAST data step of an E1d rollback (RUNBOOK-E1d
-- section 5), after the E1d units are stopped, after docs/runbooks/5035-undo.sql
-- if 5035 was applied, and after the optional backfill reverse (the reverse
-- matches rows on their `group` after-pair, so it must run first):
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/e1c-rollback-vocabulary.sql
--
-- The E1c binary decodes a synthesis visibility of `private`, `shared` or
-- `public` only, and one undecodable row fails a whole list. The E1d binary
-- writes `group` (its default), so every `group` synthesis becomes
-- `private`: the E1c meaning closest to it (readable by its author only),
-- never wider. The owner columns stay (the expand step's schema); E1c
-- ignores them.
--
-- "Never wider" holds for SYNTHESES only. E1c reads samples, protocols and
-- blobs without any ownership filter (every token holder reads them), and
-- countersignatures by claim, so a row the E1d binary wrote as `group` in
-- one of those four tables becomes readable by every token holder once the
-- E1c binary runs. The script therefore first PRINTS, per table, how many
-- such `group` rows exist (a read-only SELECT; psql shows it as a table).
-- Read the numbers before starting the previous binary: a non-zero count is
-- a decision for the operator (for example delete or keep those rows, or do
-- not roll back), not something this script decides.
--
-- Refuses while 5035 is applied (its CHECK admits `public`/`group` only).

DO $guard$
BEGIN
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5035) THEN
        RAISE EXCEPTION '5035 is still recorded; run docs/runbooks/5035-undo.sql first';
    END IF;
END $guard$;

-- Rows the previous binary will show every token holder (read-only).
SELECT t.table_name, t.group_rows
  FROM (VALUES
        ('samples',           (SELECT count(*) FROM public.samples           WHERE visibility = 'group')),
        ('protocols',         (SELECT count(*) FROM public.protocols         WHERE visibility = 'group')),
        ('blobs',             (SELECT count(*) FROM public.blobs             WHERE visibility = 'group')),
        ('countersignatures', (SELECT count(*) FROM public.countersignatures WHERE visibility = 'group'))
       ) AS t(table_name, group_rows)
 ORDER BY t.table_name;

UPDATE public.syntheses SET visibility = 'private' WHERE visibility = 'group';
