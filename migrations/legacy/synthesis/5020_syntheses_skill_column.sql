-- 5020_syntheses_skill_column.sql
-- Persist the synthesis skill used to drive the pipeline. NULL means
-- "baseline" was used (the default before this column existed).
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS skill_name TEXT;

-- New rows default to 'baseline' so we can drop the NULL = baseline
-- ambiguity once existing rows are backfilled. Backfill is below.
ALTER TABLE syntheses
    ALTER COLUMN skill_name SET DEFAULT 'baseline';

UPDATE syntheses SET skill_name = 'baseline' WHERE skill_name IS NULL;

ALTER TABLE syntheses
    ALTER COLUMN skill_name SET NOT NULL;

-- Constrain to currently-registered skills. Adding a new skill is a new
-- migration that extends this list (deliberate co-evolution).
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_skill_name_known
    CHECK (skill_name IN ('baseline'));
