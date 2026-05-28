-- 5025_protocols_section_vocabulary.sql
-- Phase 9 of the SciLink-lessons plan. Add a `sections` JSONB column to
-- protocols. The map's keys come from a fixed section vocabulary
-- (overview / planning / implementation / interpretation / validation),
-- mirroring SciLink's foundation-agent section pattern. Off-vocabulary
-- keys are preserved verbatim under "extras" by the API layer (mirrors
-- SciLink's loader warning behaviour); they're still in the JSONB.
ALTER TABLE protocols
    ADD COLUMN IF NOT EXISTS sections JSONB NOT NULL DEFAULT '{}'::jsonb;
