-- CI seed data for EpiScience's test template (applied by scripts/e1-test-db.sh
-- AFTER the kernel schema and `episcience-migrate run`, so every per-test clone
-- carries it).
--
--   agent f3951e28-9356-42b6-9c80-27dd9f01b19d  episcience-service-test
--   claim aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa  "origami melts at 50C" truth=0.8
--   claim bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb  "origami melts at 60C" truth=0.85
--
-- Tenancy: the agent gets its personal group through the kernel's own
-- `epigraph_ensure_personal_group`, and both claims DECLARE their pair
-- (`public`, owned by that personal group). No undeclared claims insert: on a
-- superuser session the kernel would otherwise stamp the seed sentinel group,
-- which is not what any production row looks like.
--
-- Idempotent (ON CONFLICT DO NOTHING).

INSERT INTO public.agents (id, public_key, display_name, agent_type, role, state)
VALUES (
    'f3951e28-9356-42b6-9c80-27dd9f01b19d',
    '\x0000000000000000000000000000000000000000000000000000000000000000',
    'episcience-service-test',
    'service',
    'custom',
    'active'
)
ON CONFLICT (id) DO NOTHING;

SELECT public.epigraph_ensure_personal_group('f3951e28-9356-42b6-9c80-27dd9f01b19d');

INSERT INTO public.claims (id, content_hash, content, truth_value, agent_id, owner_group_id, visibility)
SELECT v.id, v.hash, v.content, v.truth, 'f3951e28-9356-42b6-9c80-27dd9f01b19d', g.id, 'public'
  FROM (VALUES
        ('aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa'::uuid,
         '\xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'::bytea,
         'origami melts at 50C', 0.8::double precision),
        ('bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb'::uuid,
         '\xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb'::bytea,
         'origami melts at 60C', 0.85::double precision)
       ) AS v(id, hash, content, truth)
  JOIN public.groups g
    ON g.did_key = 'did:epigraph:personal:f3951e28-9356-42b6-9c80-27dd9f01b19d'
   AND g.kind = 'personal'
ON CONFLICT (id) DO NOTHING;
