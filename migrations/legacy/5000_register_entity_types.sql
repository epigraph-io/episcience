-- Migration 5000 (episcience): register episcience-visible custom edge entity types
-- in the kernel entity_types registry (kernel migrations 054/055).
--
-- EVIDENCE
-- Kernel migration 055 replaced the static edges_entity_types_valid CHECK with
-- FK edges.source_type/target_type -> entity_types(type_name) and a
-- registry-driven validate_edge_reference(). On a current-kernel DB, an edge
-- type is writable ONLY if it is a row in entity_types. The kernel 054 seed
-- already registers all 23 types episcience emits at runtime (synthesis, claim,
-- agent, workflow, experiment, experiment_result, ...). The one legacy episcience
-- allowlist name that is (a) NOT kernel-seeded and (b) backed by a real table is
-- `method` (kernel-owned public.methods, kernel 001). `business_function` is
-- intentionally NOT registered: it has no backing table and its legacy validate
-- branch was missing (ELSE FALSE), so such edges were never writable -- it was a
-- dead allowlist entry.
--
-- REASONING
-- Registered non-core (is_core=false) because the kernel hijack guard forbids
-- downstream rows from claiming is_core=true; registered_by stays NULL
-- (migration-registered, not API-registered). is_optional=false because
-- public.methods is created by the kernel schema and is always present after
-- provision (owned/fail-loud). ON CONFLICT (type_name) DO NOTHING makes this a
-- no-op if the kernel later seeds `method` itself (idempotent, re-run safe).
--
-- ORDERING
-- Named 5000_ so the README provision glob `migrations/5*.sql` picks it up and
-- it sorts first in that batch (existing floor is 5001), i.e. immediately after
-- 001_initial_schema.sql. It references only kernel tables (entity_types from
-- 054, public.methods from kernel 001), both applied before any episcience
-- migration, so any 5* position is valid; 5000 keeps it earliest.
--
-- VERIFICATION
-- After kernel 054/055 + episcience 001 (override removed) + this migration:
-- SELECT type_name, is_core, is_optional FROM entity_types WHERE type_name='method';
-- -> one row (is_core=false, is_optional=false). Edge inserts with
-- source_type/target_type='method' pass edges_source_type_fkey and resolve via
-- validate_edge_reference's dynamic arm against public.methods.

INSERT INTO entity_types (type_name, schema_name, table_name, id_column, is_optional, is_core, registered_by, description)
VALUES
    ('method', 'public', 'methods', 'id', false, false, NULL,
     'Research method entity (kernel-owned public.methods table); registered by episcience so method edges remain writable under the registry-driven kernel FK/validation (kernel migrations 054/055).')
ON CONFLICT (type_name) DO NOTHING;
