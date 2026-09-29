-- 5028_syntheses_skill_literature.sql
-- EpiClaw integration Phase 2. Extend syntheses_skill_name_known to
-- permit 'literature' in addition to the previously-known skills.
ALTER TABLE syntheses
    DROP CONSTRAINT syntheses_skill_name_known;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline', 'lab_notebook', 'literature'));
