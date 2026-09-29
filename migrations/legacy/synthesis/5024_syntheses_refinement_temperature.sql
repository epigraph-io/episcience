-- 5024_syntheses_refinement_temperature.sql
-- Persist the refinement temperature so the job handler can recover the
-- annealing state across worker restarts. NULL means "this row is not a
-- refinement child" (or its parent never spawned one).
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS refinement_temperature JSONB;

-- Documenting the convention: the column should be NULL on the original
-- synthesis (depth_delta=0). Refinement children inherit the parent's
-- temperature.anneal() value, so the depth_delta on the child is the
-- temperature AT WHICH it was spawned.
