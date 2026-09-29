-- 5031_synthesis_provo_edges_target_kind_workflow.sql
--
-- Widen the synthesis_provo_edges.target_kind CHECK to allow 'workflow', so a
-- REFINES edge can point from a synthesis-refinement chain to a workflow row
-- (linking workflow-generation provenance to the synthesis that refined it).
--
-- Additive only: existing rows hold target_kind IN ('claim','synthesis','agent')
-- and remain valid under the wider set. No caller emits 'workflow' yet — the
-- staging-table enablement ships ahead of the (designed, deferred) caller so the
-- shape is available the moment a workflow<->refinement link is wired.
--
-- The constraint is single-column and was created inline/unnamed in migration
-- 5018, so Postgres auto-named it `synthesis_provo_edges_target_kind_check`.

ALTER TABLE synthesis_provo_edges
    DROP CONSTRAINT synthesis_provo_edges_target_kind_check;

ALTER TABLE synthesis_provo_edges
    ADD CONSTRAINT synthesis_provo_edges_target_kind_check
    CHECK (target_kind IN ('claim', 'synthesis', 'agent', 'workflow'));
