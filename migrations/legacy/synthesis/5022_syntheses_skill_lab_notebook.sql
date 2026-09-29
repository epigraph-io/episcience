-- 5022_syntheses_skill_lab_notebook.sql
-- Extend the syntheses_skill_name_known CHECK constraint added by
-- migration 5020 to permit 'lab_notebook' in addition to 'baseline'.
ALTER TABLE syntheses
    DROP CONSTRAINT syntheses_skill_name_known;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline', 'lab_notebook'));
