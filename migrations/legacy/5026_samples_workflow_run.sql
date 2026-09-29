-- 5026_samples_workflow_run.sql
-- Phase 1 of the EpiClaw integration. Adds a DB-level CHECK constraint
-- on samples.sample_type to mirror the SampleType enum (previously
-- enforced only at the Rust layer). The constraint includes the new
-- `workflow_run` value introduced by this phase.
ALTER TABLE samples
    ADD CONSTRAINT samples_sample_type_check
    CHECK (sample_type IN (
        'biological', 'chemical', 'material', 'composite', 'workflow_run'
    ));
