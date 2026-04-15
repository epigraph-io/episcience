-- Initial schema: epigraph-io/episcience
--
-- Collapsed from episcience migrations 001–002 (experimental loop tables,
-- dynamic Jaccard-based shared-evidence factor strength).
-- Authoritative source: EpiGraphV2 repo-split/episcience/ staging (tag repo-split/v0).
--
-- PREREQUISITE: epigraph-io/epigraph kernel schema must be applied first.
-- Specifically requires: cascade_delete_edges(), factors table, frames table,
-- methods table, validate_edge_reference() from kernel migrations 040–043.
--
-- Apply on a kernel-initialized PostgreSQL 16+ database:
--   \i 001_initial_schema.sql

-- ────────────────────────────────────────────────────────────────────────────
-- 001_experimental_loop.sql
-- ────────────────────────────────────────────────────────────────────────────

-- Migration 001 (episcience): Experimental Epistemic Loop
--
-- Extracted from EpiGraphV2 migration 049_experimental_loop.sql.
-- This migration MUST run after the kernel schema is in place
-- (specifically after the factors frame-scoping migration and the
-- base validate_edge_reference() function from kernel migration 043).
--
-- This file creates the experiment lifecycle tables and wires them into
-- the kernel's edge validation, cascade delete, and factor generation
-- infrastructure.
--
-- Evidence: EpiGraphV2 migration 049 (2026 monorepo)
-- Reasoning: experiments/experiment_results are science-domain concepts
--   that belong in the episcience product repo, not the open kernel.
--   The kernel's validate_edge_reference() and edge CHECK constraints are
--   extended here as additive migrations — no kernel files are modified.
-- Verification: sqlx migrate run on schema with kernel migrations applied

-- 1. Frames required by hypothesis tracking
--    (idempotent — kernel may have inserted these already via bootstrap)
INSERT INTO frames (name, description, hypotheses)
VALUES (
    'hypothesis_assessment',
    'Binary frame for evaluating hypotheses: supported vs unsupported',
    ARRAY['supported', 'unsupported']
)
ON CONFLICT (name) DO NOTHING;

INSERT INTO frames (name, description, hypotheses)
VALUES (
    'research_validity',
    'Standard frame for research claim validity assessment',
    ARRAY['supported', 'unsupported']
)
ON CONFLICT (name) DO NOTHING;

-- 2. experiments table
CREATE TABLE IF NOT EXISTS experiments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    hypothesis_id   UUID NOT NULL REFERENCES claims(id),
    created_by      UUID NOT NULL REFERENCES agents(id),
    method_ids      UUID[],
    protocol        TEXT,
    protocol_source JSONB,
    status          VARCHAR(20) NOT NULL DEFAULT 'designed'
                    CHECK (status IN ('designed','running','collecting','analyzing','complete','failed')),
    started_at      TIMESTAMPTZ,
    completed_at    TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_experiments_hypothesis  ON experiments(hypothesis_id);
CREATE INDEX IF NOT EXISTS idx_experiments_status      ON experiments(status);
CREATE INDEX IF NOT EXISTS idx_experiments_created_by  ON experiments(created_by);

-- 3. experiment_results table
CREATE TABLE IF NOT EXISTS experiment_results (
    id                     UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    experiment_id          UUID NOT NULL REFERENCES experiments(id) ON DELETE CASCADE,
    data_source            VARCHAR(30) NOT NULL CHECK (data_source IN ('manual','simulation','instrument','literature','computed')),
    raw_measurements       JSONB NOT NULL DEFAULT '[]',
    measurement_count      INT NOT NULL DEFAULT 0,
    effective_random_error JSONB,
    processed_data         JSONB,
    status                 VARCHAR(20) NOT NULL DEFAULT 'pending'
                           CHECK (status IN ('pending','processing','complete','error')),
    created_at             TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_experiment_results_experiment ON experiment_results(experiment_id);
CREATE INDEX IF NOT EXISTS idx_experiment_results_status     ON experiment_results(status);

-- 4. Extend the kernel's edges entity_type CHECK constraint to include
--    experiment and experiment_result entity types.
ALTER TABLE edges DROP CONSTRAINT IF EXISTS edges_entity_types_valid;
ALTER TABLE edges ADD CONSTRAINT edges_entity_types_valid CHECK (
    source_type IN ('claim','agent','evidence','trace','node','activity',
                    'paper','perspective','community','context','frame',
                    'experiment','experiment_result','analysis',
                    'source_artifact','task','event',
                    'propaganda_technique','coalition',
                    'method','business_function') AND
    target_type IN ('claim','agent','evidence','trace','node','activity',
                    'paper','perspective','community','context','frame',
                    'experiment','experiment_result','analysis',
                    'source_artifact','task','event',
                    'propaganda_technique','coalition',
                    'method','business_function')
);

-- 5. Extend validate_edge_reference() to handle experiment entity types
CREATE OR REPLACE FUNCTION validate_edge_reference(
    entity_id UUID,
    entity_type VARCHAR
) RETURNS BOOLEAN AS $$
BEGIN
    RETURN CASE entity_type
        WHEN 'claim'              THEN EXISTS (SELECT 1 FROM claims            WHERE id = entity_id)
        WHEN 'agent'              THEN EXISTS (SELECT 1 FROM agents            WHERE id = entity_id)
        WHEN 'evidence'           THEN EXISTS (SELECT 1 FROM evidence          WHERE id = entity_id)
        WHEN 'trace'              THEN EXISTS (SELECT 1 FROM reasoning_traces  WHERE id = entity_id)
        WHEN 'paper'              THEN EXISTS (SELECT 1 FROM papers            WHERE id = entity_id)
        WHEN 'analysis'           THEN EXISTS (SELECT 1 FROM analyses          WHERE id = entity_id)
        WHEN 'experiment'         THEN EXISTS (SELECT 1 FROM experiments       WHERE id = entity_id)
        WHEN 'experiment_result'  THEN EXISTS (SELECT 1 FROM experiment_results WHERE id = entity_id)
        WHEN 'perspective'        THEN EXISTS (SELECT 1 FROM perspectives      WHERE id = entity_id)
        WHEN 'community'          THEN EXISTS (SELECT 1 FROM communities       WHERE id = entity_id)
        WHEN 'context'            THEN EXISTS (SELECT 1 FROM contexts          WHERE id = entity_id)
        WHEN 'frame'              THEN EXISTS (SELECT 1 FROM frames            WHERE id = entity_id)
        WHEN 'activity'           THEN EXISTS (SELECT 1 FROM activities        WHERE id = entity_id)
        WHEN 'source_artifact'    THEN EXISTS (SELECT 1 FROM source_artifacts  WHERE id = entity_id)
        WHEN 'method'             THEN EXISTS (SELECT 1 FROM methods           WHERE id = entity_id)
        WHEN 'node'               THEN TRUE
        ELSE FALSE
    END;
END;
$$ LANGUAGE plpgsql STABLE;

-- 6. Cascade delete triggers for experiment tables
CREATE TRIGGER experiments_cascade_edges
    BEFORE DELETE ON experiments
    FOR EACH ROW EXECUTE FUNCTION cascade_delete_edges('experiment');

CREATE TRIGGER experiment_results_cascade_edges
    BEFORE DELETE ON experiment_results
    FOR EACH ROW EXECUTE FUNCTION cascade_delete_edges('experiment_result');

-- 7. Shared-evidence factor creation for analyses that span multiple claims
--    (used by hypothesis evaluation: when two claims share the same analysis
--    as evidence, a shared_evidence factor connects them in the factor graph)
CREATE OR REPLACE FUNCTION create_shared_evidence_factor()
RETURNS TRIGGER AS $$
DECLARE
    other_claim_id UUID;
    hyp_frame_id UUID;
    var_ids UUID[];
BEGIN
    -- Only for provides_evidence edges from analysis to claim
    IF NEW.relationship != 'provides_evidence'
       OR NEW.source_type != 'analysis'
       OR NEW.target_type != 'claim' THEN
        RETURN NEW;
    END IF;

    SELECT id INTO hyp_frame_id FROM frames WHERE name = 'hypothesis_assessment' LIMIT 1;

    -- Find all other claims this analysis already provides_evidence to
    FOR other_claim_id IN
        SELECT target_id FROM edges
        WHERE source_id = NEW.source_id
          AND source_type = 'analysis'
          AND target_type = 'claim'
          AND relationship = 'provides_evidence'
          AND target_id != NEW.target_id
    LOOP
        -- Build sorted variable_ids
        IF NEW.target_id < other_claim_id THEN
            var_ids := ARRAY[NEW.target_id, other_claim_id];
        ELSE
            var_ids := ARRAY[other_claim_id, NEW.target_id];
        END IF;

        INSERT INTO factors (factor_type, variable_ids, potential, description, properties, frame_id)
        VALUES (
            'shared_evidence',
            var_ids,
            jsonb_build_object('strength', 0.7),
            format('Shared evidence via analysis %s', NEW.source_id),
            jsonb_build_object('analysis_id', NEW.source_id),
            hyp_frame_id
        )
        ON CONFLICT (factor_type, variable_ids, COALESCE(frame_id, '00000000-0000-0000-0000-000000000000'))
        DO NOTHING;
    END LOOP;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER edges_shared_evidence
    AFTER INSERT ON edges
    FOR EACH ROW
    EXECUTE FUNCTION create_shared_evidence_factor();

-- ────────────────────────────────────────────────────────────────────────────
-- 002_dynamic_shared_evidence_strength.sql
-- ────────────────────────────────────────────────────────────────────────────

-- Replace create_shared_evidence_factor() with Jaccard similarity on method parameter keys.
-- Previously hardcoded strength = 0.7; now computes overlap of typical_conditions keys.

CREATE OR REPLACE FUNCTION create_shared_evidence_factor()
RETURNS TRIGGER AS $$
DECLARE
    other_claim_id UUID;
    hyp_frame_id UUID;
    var_ids UUID[];
    keys_new TEXT[];
    keys_other TEXT[];
    keys_union TEXT[];
    keys_intersect TEXT[];
    jaccard FLOAT;
    factor_strength FLOAT;
BEGIN
    -- Only for provides_evidence edges from analysis to claim
    IF NEW.relationship != 'provides_evidence'
       OR NEW.source_type != 'analysis'
       OR NEW.target_type != 'claim' THEN
        RETURN NEW;
    END IF;

    SELECT id INTO hyp_frame_id FROM frames WHERE name = 'hypothesis_assessment' LIMIT 1;

    -- Collect method parameter keys for the NEW claim (union across all experiments + methods)
    SELECT COALESCE(array_agg(DISTINCT k), ARRAY[]::TEXT[])
    INTO keys_new
    FROM experiments e
    CROSS JOIN LATERAL unnest(e.method_ids) AS mid
    JOIN methods m ON m.id = mid
    CROSS JOIN LATERAL jsonb_object_keys(COALESCE(m.typical_conditions, '{}'::jsonb)) AS k
    WHERE e.hypothesis_id = NEW.target_id;

    -- Find all other claims this analysis already provides_evidence to
    FOR other_claim_id IN
        SELECT target_id FROM edges
        WHERE source_id = NEW.source_id
          AND source_type = 'analysis'
          AND target_type = 'claim'
          AND relationship = 'provides_evidence'
          AND target_id != NEW.target_id
    LOOP
        -- Build sorted variable_ids
        IF NEW.target_id < other_claim_id THEN
            var_ids := ARRAY[NEW.target_id, other_claim_id];
        ELSE
            var_ids := ARRAY[other_claim_id, NEW.target_id];
        END IF;

        -- Collect method parameter keys for the OTHER claim
        SELECT COALESCE(array_agg(DISTINCT k), ARRAY[]::TEXT[])
        INTO keys_other
        FROM experiments e
        CROSS JOIN LATERAL unnest(e.method_ids) AS mid
        JOIN methods m ON m.id = mid
        CROSS JOIN LATERAL jsonb_object_keys(COALESCE(m.typical_conditions, '{}'::jsonb)) AS k
        WHERE e.hypothesis_id = other_claim_id;

        -- Compute Jaccard similarity
        IF array_length(keys_new, 1) IS NULL OR array_length(keys_other, 1) IS NULL THEN
            -- No method keys available, fall back to 0.7
            factor_strength := 0.7;
        ELSE
            -- Union = all distinct keys from both
            SELECT COALESCE(array_agg(DISTINCT x), ARRAY[]::TEXT[])
            INTO keys_union
            FROM (
                SELECT unnest(keys_new) AS x
                UNION
                SELECT unnest(keys_other)
            ) sub;

            -- Intersection = keys present in both
            SELECT COALESCE(array_agg(x), ARRAY[]::TEXT[])
            INTO keys_intersect
            FROM (
                SELECT unnest(keys_new) AS x
                INTERSECT
                SELECT unnest(keys_other)
            ) sub;

            IF array_length(keys_union, 1) IS NULL OR array_length(keys_union, 1) = 0 THEN
                factor_strength := 0.7;
            ELSE
                jaccard := array_length(keys_intersect, 1)::FLOAT / array_length(keys_union, 1)::FLOAT;
                factor_strength := GREATEST(0.3, jaccard);
            END IF;
        END IF;

        -- Create pairwise shared_evidence factor with computed strength
        INSERT INTO factors (factor_type, variable_ids, potential, description, properties, frame_id)
        VALUES (
            'shared_evidence',
            var_ids,
            jsonb_build_object('strength', factor_strength),
            format('Shared evidence via analysis %s (Jaccard=%s)', NEW.source_id, ROUND(COALESCE(jaccard, 0.7)::numeric, 3)),
            jsonb_build_object('analysis_id', NEW.source_id, 'jaccard_similarity', COALESCE(jaccard, 0.7)),
            hyp_frame_id
        )
        ON CONFLICT (factor_type, variable_ids, COALESCE(frame_id, '00000000-0000-0000-0000-000000000000'))
        DO UPDATE SET
            potential = jsonb_build_object('strength', factor_strength),
            description = format('Shared evidence via analysis %s (Jaccard=%s)', NEW.source_id, ROUND(COALESCE(jaccard, 0.7)::numeric, 3)),
            properties = jsonb_build_object('analysis_id', NEW.source_id, 'jaccard_similarity', COALESCE(jaccard, 0.7));
    END LOOP;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Recompute existing shared_evidence factors from current method data
UPDATE factors f
SET potential = jsonb_build_object('strength', sub.strength),
    properties = f.properties || jsonb_build_object('jaccard_similarity', sub.jaccard)
FROM (
    SELECT
        f2.id AS factor_id,
        CASE
            WHEN COALESCE(array_length(union_keys, 1), 0) = 0 THEN 0.7
            ELSE GREATEST(0.3, COALESCE(array_length(intersect_keys, 1), 0)::FLOAT / array_length(union_keys, 1)::FLOAT)
        END AS strength,
        CASE
            WHEN COALESCE(array_length(union_keys, 1), 0) = 0 THEN 0.7
            ELSE COALESCE(array_length(intersect_keys, 1), 0)::FLOAT / array_length(union_keys, 1)::FLOAT
        END AS jaccard
    FROM factors f2
    CROSS JOIN LATERAL (
        SELECT
            (SELECT COALESCE(array_agg(DISTINCT k), ARRAY[]::TEXT[])
             FROM experiments e
             CROSS JOIN LATERAL unnest(e.method_ids) AS mid
             JOIN methods m ON m.id = mid
             CROSS JOIN LATERAL jsonb_object_keys(COALESCE(m.typical_conditions, '{}'::jsonb)) AS k
             WHERE e.hypothesis_id = f2.variable_ids[1]
            ) AS keys_a,
            (SELECT COALESCE(array_agg(DISTINCT k), ARRAY[]::TEXT[])
             FROM experiments e
             CROSS JOIN LATERAL unnest(e.method_ids) AS mid
             JOIN methods m ON m.id = mid
             CROSS JOIN LATERAL jsonb_object_keys(COALESCE(m.typical_conditions, '{}'::jsonb)) AS k
             WHERE e.hypothesis_id = f2.variable_ids[2]
            ) AS keys_b
    ) keys
    CROSS JOIN LATERAL (
        SELECT
            (SELECT array_agg(DISTINCT x) FROM (SELECT unnest(keys.keys_a) AS x UNION SELECT unnest(keys.keys_b)) u) AS union_keys,
            (SELECT array_agg(x) FROM (SELECT unnest(keys.keys_a) AS x INTERSECT SELECT unnest(keys.keys_b)) i) AS intersect_keys
    ) computed
    WHERE f2.factor_type = 'shared_evidence'
) sub
WHERE f.id = sub.factor_id
  AND f.factor_type = 'shared_evidence';

