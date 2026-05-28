-- 5030_syntheses_skill_registry_diff.sql
-- EpiClaw integration Phase 4. Extend syntheses_skill_name_known to
-- permit 'registry_diff' alongside the previously-known skills.
ALTER TABLE syntheses
    DROP CONSTRAINT syntheses_skill_name_known;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline', 'lab_notebook', 'literature', 'code_review', 'registry_diff'));
