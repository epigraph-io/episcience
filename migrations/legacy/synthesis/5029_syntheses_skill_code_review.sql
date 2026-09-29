-- 5029_syntheses_skill_code_review.sql
-- EpiClaw integration Phase 3. Extend syntheses_skill_name_known to
-- permit 'code_review' in addition to the previously-known skills.
ALTER TABLE syntheses
    DROP CONSTRAINT syntheses_skill_name_known;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline', 'lab_notebook', 'literature', 'code_review'));
