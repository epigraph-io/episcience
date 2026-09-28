-- docs/runbooks/5040-undo.sql -- compensating SQL for E1h's migration 5040:
-- puts the legacy `edges_shared_evidence` trigger back on the kernel's
-- `edges` table.
--
-- NOT a migration (the ledger is forward-only), and run ONLY on the
-- operator's request. Run by the operator, as the migration owner, in ONE
-- transaction:
--
--   psql -X -v ON_ERROR_STOP=1 --single-transaction -f docs/runbooks/5040-undo.sql
--
-- It recreates `public.create_shared_evidence_factor()` and the AFTER INSERT
-- trigger `edges_shared_evidence` on `public.edges` exactly as the last
-- definition in `migrations/legacy/001_initial_schema.sql` (its section 002,
-- "dynamic shared evidence strength") created them, and deletes the 5040
-- ledger row, so a later `episcience-migrate run` detaches them again. If the
-- definitions were captured from the live database before 5040 ran, prefer
-- those (they are the authoritative pre-5040 state).
--
-- On a database that never had the trigger (every database built from the
-- 5032 baseline) this ADDS it: do not run it there.
--
-- Refuses unless 5040 is recorded, while a later EpiScience migration is
-- recorded, and while the trigger or the function already exists.

SET LOCAL lock_timeout = '5s';

DO $guard$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version = 5040) THEN
        RAISE EXCEPTION '5040 is not recorded; nothing to undo';
    END IF;
    IF EXISTS (SELECT 1 FROM episcience_meta._sqlx_migrations WHERE version > 5040) THEN
        RAISE EXCEPTION 'a later EpiScience migration is recorded; undo it first';
    END IF;
    IF EXISTS (SELECT 1 FROM pg_catalog.pg_trigger
                WHERE tgrelid = 'public.edges'::regclass AND tgname = 'edges_shared_evidence')
       OR pg_catalog.to_regprocedure('public.create_shared_evidence_factor()') IS NOT NULL THEN
        RAISE EXCEPTION 'edges_shared_evidence or create_shared_evidence_factor() already exists; nothing changed';
    END IF;
END $guard$;

-- ─── The function (legacy 001, section 002, verbatim but for the schema) ───
CREATE OR REPLACE FUNCTION public.create_shared_evidence_factor()
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

-- ─── The trigger (legacy 001, verbatim but for the schema) ──────────────────
CREATE TRIGGER edges_shared_evidence
    AFTER INSERT ON public.edges
    FOR EACH ROW
    EXECUTE FUNCTION public.create_shared_evidence_factor();

DELETE FROM episcience_meta._sqlx_migrations WHERE version = 5040;
