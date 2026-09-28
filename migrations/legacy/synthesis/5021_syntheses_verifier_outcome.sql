-- 5021_syntheses_verifier_outcome.sql
-- Stage 6 (verifier) persistence: row records the verifier's outcome
-- so post-hoc inspection can see why a synthesis was accepted or rejected,
-- plus a counter for refinement chains (Task 7.1).
ALTER TABLE syntheses
    ADD COLUMN IF NOT EXISTS verifier_outcome JSONB,
    ADD COLUMN IF NOT EXISTS verifier_attempts SMALLINT NOT NULL DEFAULT 0;

-- Status lifecycle gains two new states:
-- 'verifying' — between compose and publish (run by the worker, transient).
-- 'rejected' — verifier rejected; row will not be published. Phase 7 may
-- spawn a refinement child via PROV-O REFINES.
ALTER TABLE syntheses
    DROP CONSTRAINT IF EXISTS syntheses_status_check;
ALTER TABLE syntheses
    ADD CONSTRAINT syntheses_status_check
    CHECK (status IN (
        'pending', 'running', 'verifying',
        'complete', 'failed', 'deleted', 'rejected'
    ));
