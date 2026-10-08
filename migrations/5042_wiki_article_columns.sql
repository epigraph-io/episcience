SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

-- The lock timeout: adding the CHECKs takes an ACCESS EXCLUSIVE lock on
-- `syntheses` while they validate the existing rows (every row passes: both
-- columns are new and NULL), and the running API and worker write that
-- table. On a busy table the migration gives up after 5 s with nothing
-- applied (re-run it) instead of queueing every synthesis write behind it.

-- 5042_wiki_article_columns.sql -- wiki articles are syntheses (plan
-- 2026-10-08-wiki-phase-b-articles.md, Task 4). The `skill_name` CHECK
-- already admits 'wiki_article' (5041).
--
-- WHAT: two nullable columns on `public.syntheses`:
--   seed_theme_id: the `claim_themes` row the article's Stage 1 seed was drawn
--     from. Provenance only, no FK: themes are re-projected with new ids.
--   wiki_key: the page key from the theme's clustering provenance
--     (`episcience_core::wiki::WikiKey::as_slug`: `r<run id, 32 hex>-c<cluster
--     id>` plus `-s<split part>` when present), never the theme UUID.
-- `syntheses_wiki_key_shape` admits only that slug shape (Task 5's page route
-- parses it back); `syntheses_wiki_pair` keeps the two set together. The
-- partial index serves the page registry (latest row per owner group and
-- key) without touching rows that are not wiki articles.
--
-- WHY: the wiki registry is a read over `syntheses`, so it inherits the
-- existing row security and tenancy guards with no new tenant table.
--
-- EFFECT: no existing row changes (both columns NULL). Undo, on operator
-- request only: docs/runbooks/5042-undo.sql (drops the page keys).
ALTER TABLE public.syntheses
    ADD COLUMN seed_theme_id uuid,
    ADD COLUMN wiki_key text;

ALTER TABLE public.syntheses
    ADD CONSTRAINT syntheses_wiki_key_shape
        CHECK (wiki_key IS NULL OR wiki_key ~ '^r[0-9a-f]{32}-c[0-9]+(-s[0-9]+)?$'),
    ADD CONSTRAINT syntheses_wiki_pair
        CHECK ((wiki_key IS NULL) = (seed_theme_id IS NULL));

CREATE INDEX syntheses_wiki_page_idx
    ON public.syntheses (owner_group_id, wiki_key, created_at DESC)
    WHERE wiki_key IS NOT NULL;
