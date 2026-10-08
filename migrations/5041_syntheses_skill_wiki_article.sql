SELECT public.episcience_assert_kernel_contract(1);
SET LOCAL lock_timeout = '5s';

-- The lock timeout: re-adding the CHECK takes an ACCESS EXCLUSIVE lock on
-- `syntheses` while it validates the existing rows, and the running API and
-- worker write that table. On a busy table the migration gives up after 5 s
-- with nothing applied (re-run it) instead of queueing every synthesis write
-- behind it.

-- 5041_syntheses_skill_wiki_article.sql -- the `wiki_article` synthesis skill
-- becomes a value `syntheses.skill_name` accepts.
--
-- WHAT: widens `syntheses_skill_name_known` (5032 baseline, last widened by
-- the hand-applied legacy 5030) from the five skills that shipped before it to
-- those five plus `wiki_article` (`episcience_core::wiki::WIKI_SKILL_NAME`,
-- registered in `episcience_core::synthesis::skills`).
--
-- WHY: every skill ships with its CHECK widening (legacy 5022, 5028, 5029,
-- 5030). Without it every wiki article insert (`create_pending_tx` with
-- `skill_name = 'wiki_article'`) fails the CHECK, so no article can be
-- written (plan 2026-10-08-wiki-phase-b-articles.md). The constraint stays
-- closed: dropping it would let a typo persist a row no skill can run.
-- `episcience-db/tests/synthesis_repo_test.rs`
-- (`every_registered_skill_name_is_accepted_and_others_are_refused`) fails
-- when a registered skill is missing here.
--
-- EFFECT: no existing row changes (every stored name is one of the five,
-- which stay allowed). Undo, on operator request only and only while no
-- `wiki_article` row exists: re-add the CHECK without `wiki_article`.
ALTER TABLE public.syntheses
    DROP CONSTRAINT syntheses_skill_name_known,
    ADD CONSTRAINT syntheses_skill_name_known
        CHECK (skill_name = ANY (ARRAY['baseline', 'lab_notebook', 'literature', 'code_review', 'registry_diff', 'wiki_article']));
