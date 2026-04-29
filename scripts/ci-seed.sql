-- CI seed data for episcience integration tests.
--
-- This seeds the minimum fixtures the test suite requires after the upstream +
-- episcience + synthesis migrations have been applied. Mirrors the dev-DB seed
-- documented in P3/P5 validation:
--
--   agent f3951e28-9356-42b6-9c80-27dd9f01b19d  episcience-service-test
--   claim aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa  "origami melts at 50C" truth=0.8
--   claim bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb  "origami melts at 60C" truth=0.85
--
-- These fixtures back the following tests:
--   - episcience-db: synthesis_pipeline_stage1_test::stage1_seed_returns_recall_results
--   - episcience-api: phase01_e2e_test::test_phase0_library_get_belief_callable
--   - episcience-api: phase01_e2e_test::test_phase0_real_edge_emits_event_in_db (also needs running upstream API)
--
-- All inserts use ON CONFLICT DO NOTHING so the script is idempotent.

INSERT INTO agents (id, public_key, display_name, agent_type, role, state)
VALUES (
    'f3951e28-9356-42b6-9c80-27dd9f01b19d',
    '\x0000000000000000000000000000000000000000000000000000000000000000',
    'episcience-service-test',
    'service',
    'custom',
    'active'
)
ON CONFLICT (id) DO NOTHING;

INSERT INTO claims (id, content_hash, content, truth_value, agent_id)
VALUES (
    'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa',
    '\xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
    'origami melts at 50C',
    0.8,
    'f3951e28-9356-42b6-9c80-27dd9f01b19d'
)
ON CONFLICT (id) DO NOTHING;

INSERT INTO claims (id, content_hash, content, truth_value, agent_id)
VALUES (
    'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb',
    '\xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
    'origami melts at 60C',
    0.85,
    'f3951e28-9356-42b6-9c80-27dd9f01b19d'
)
ON CONFLICT (id) DO NOTHING;
