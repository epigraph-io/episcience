-- 5023_syntheses_novelty.sql
-- Stage 7 (novelty) persistence: row records the novelty score and
-- the backend that produced it. NULL until Stage 7 runs (only after
-- a successful Stage 6 accept).
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS novelty_score JSONB,
    ADD COLUMN IF NOT EXISTS novelty_backend TEXT;
