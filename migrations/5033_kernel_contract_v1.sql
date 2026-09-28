DO $contract$
BEGIN
    -- >>> contract v1 checks
    -- The EpiGraph kernel objects EpiScience SQL may reference (tenancy
    -- contract v1, docs/tenancy-contract.md), plus the legacy EpiScience head
    -- (L1). Every item is present at kernel migration head 110. The first
    -- missing item RAISEs, naming it ("kernel contract v1: C<n> ..."), so a
    -- migration never half-applies on a kernel that drifted.
    --
    -- This region appears twice, verbatim: in the DO block that opens this
    -- migration (it runs before the function exists) and in
    -- public.episcience_assert_kernel_contract. A test compares the copies.
    -- Existence is checked before any privilege: the has_*_privilege
    -- functions ERROR on a missing role or column instead of returning false.
    DECLARE
        v_role     text;
        v_name     text;
        v_col      text;
        v_rel      regclass;
        v_proc     regprocedure;
        v_head     bigint;
        v_ext_nsp  text;
        v_spec     text[];
    BEGIN
        -- C1: the kernel roles.
        FOREACH v_role IN ARRAY ARRAY['epigraph_app', 'epigraph_maintenance'] LOOP
            IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = v_role) THEN
                RAISE EXCEPTION 'kernel contract v1: C1 failed: role % is missing', v_role
                    USING HINT = 'EpiScience needs the kernel tenancy roles (kernel migration head >= 110).';
            END IF;
        END LOOP;

        -- C2: the session functions, their result types, EXECUTE by epigraph_app.
        FOREACH v_spec SLICE 1 IN ARRAY ARRAY[
            ARRAY['epigraph_bypass',          'boolean'],
            ARRAY['epigraph_definer_bypass',  'boolean'],
            ARRAY['epigraph_session_groups',  'uuid[]'],
            ARRAY['epigraph_writable_groups', 'uuid[]'],
            ARRAY['epigraph_principal_id',    'uuid']
        ] LOOP
            v_proc := pg_catalog.to_regprocedure('public.' || v_spec[1] || '()');
            IF v_proc IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: C2 failed: function public.%() is missing', v_spec[1];
            END IF;
            IF pg_catalog.pg_get_function_result(v_proc) IS DISTINCT FROM v_spec[2] THEN
                RAISE EXCEPTION 'kernel contract v1: C2 failed: public.%() returns %, expected %',
                    v_spec[1], pg_catalog.pg_get_function_result(v_proc), v_spec[2];
            END IF;
            IF NOT pg_catalog.has_function_privilege('epigraph_app', v_proc, 'EXECUTE') THEN
                RAISE EXCEPTION 'kernel contract v1: C2 failed: epigraph_app cannot EXECUTE public.%()', v_spec[1];
            END IF;
        END LOOP;

        -- C3, C4, C5, C13: the kernel columns EpiScience reads or writes.
        FOREACH v_spec SLICE 1 IN ARRAY ARRAY[
            ARRAY['C3',  'groups',            'id'],
            ARRAY['C3',  'groups',            'kind'],
            ARRAY['C3',  'groups',            'did_key'],
            ARRAY['C3',  'groups',            'created_by_agent_id'],
            ARRAY['C3',  'group_memberships', 'group_id'],
            ARRAY['C3',  'group_memberships', 'agent_id'],
            ARRAY['C3',  'group_memberships', 'role'],
            ARRAY['C3',  'group_memberships', 'revoked_at'],
            ARRAY['C4',  'claims',            'id'],
            ARRAY['C4',  'claims',            'visibility'],
            ARRAY['C4',  'claims',            'owner_group_id'],
            ARRAY['C5',  'security_events',   'event_type'],
            ARRAY['C5',  'security_events',   'agent_id'],
            ARRAY['C5',  'security_events',   'success'],
            ARRAY['C5',  'security_events',   'details'],
            ARRAY['C13', 'agents',            'id'],
            ARRAY['C13', 'agents',            'public_key'],
            ARRAY['C13', 'agents',            'display_name']
        ] LOOP
            v_rel := pg_catalog.to_regclass('public.' || v_spec[2]);
            IF v_rel IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: % failed: table public.% is missing', v_spec[1], v_spec[2];
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                            WHERE a.attrelid = v_rel AND a.attname = v_spec[3]
                              AND a.attnum > 0 AND NOT a.attisdropped) THEN
                RAISE EXCEPTION 'kernel contract v1: % failed: column public.%.% is missing',
                    v_spec[1], v_spec[2], v_spec[3];
            END IF;
        END LOOP;

        -- C5: the maintenance role appends audit rows.
        IF NOT pg_catalog.has_table_privilege('epigraph_maintenance', 'public.security_events'::regclass, 'INSERT') THEN
            RAISE EXCEPTION 'kernel contract v1: C5 failed: epigraph_maintenance cannot INSERT into public.security_events';
        END IF;

        -- C6: the world and seed sentinel groups.
        IF NOT EXISTS (SELECT 1 FROM public.groups g
                        WHERE g.id = '00000000-0000-0000-0000-000000000000'::uuid AND g.kind = 'world') THEN
            RAISE EXCEPTION 'kernel contract v1: C6 failed: the world sentinel group is missing';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM public.groups g
                        WHERE g.id = '00000000-0000-0000-0000-00000000dead'::uuid AND g.kind = 'seed') THEN
            RAISE EXCEPTION 'kernel contract v1: C6 failed: the seed sentinel group is missing';
        END IF;

        -- C7: the kernel ledger (read-only) is at head 110 or later. The head
        -- is taken over kernel-range versions only (below 5000, the floor of
        -- EpiScience's range), so a foreign row cannot raise it, and version
        -- 110 itself must be recorded as applied, so an out-of-range row
        -- below the floor cannot stand in for it either.
        v_rel := pg_catalog.to_regclass('public._sqlx_migrations');
        IF v_rel IS NULL THEN
            RAISE EXCEPTION 'kernel contract v1: C7 failed: the kernel ledger public._sqlx_migrations is missing';
        END IF;
        SELECT max(m.version) INTO v_head FROM public._sqlx_migrations m
         WHERE m.success AND m.version < 5000;
        IF v_head IS NULL OR v_head < 110 THEN
            RAISE EXCEPTION 'kernel contract v1: C7 failed: kernel migration head is %, contract v1 needs >= 110',
                coalesce(v_head::text, 'none')
                USING HINT = 'Run the kernel''s epigraph-migrate first.';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM public._sqlx_migrations m WHERE m.version = 110 AND m.success) THEN
            RAISE EXCEPTION 'kernel contract v1: C7 failed: kernel migration 110 is not recorded as applied'
                USING HINT = 'Run the kernel''s epigraph-migrate first.';
        END IF;

        -- C8: the entity-type registration of syntheses (read-only).
        IF pg_catalog.to_regclass('public.entity_types') IS NULL
           OR NOT EXISTS (SELECT 1 FROM public.entity_types e
                           WHERE e.type_name = 'synthesis' AND e.table_name = 'syntheses') THEN
            RAISE EXCEPTION 'kernel contract v1: C8 failed: public.entity_types has no synthesis -> syntheses row';
        END IF;

        -- C9: pgvector, in schema public (5032 qualifies its types as public.vector).
        SELECT n.nspname INTO v_ext_nsp
          FROM pg_catalog.pg_extension x
          JOIN pg_catalog.pg_namespace n ON n.oid = x.extnamespace
         WHERE x.extname = 'vector';
        IF v_ext_nsp IS DISTINCT FROM 'public' THEN
            RAISE EXCEPTION 'kernel contract v1: C9 failed: extension vector is in schema %, expected public',
                coalesce(v_ext_nsp, '(not installed)');
        END IF;

        -- C10: the maintenance role reads claims and groups.
        FOREACH v_name IN ARRAY ARRAY['claims', 'groups', 'group_memberships'] LOOP
            IF NOT pg_catalog.has_table_privilege('epigraph_maintenance', ('public.' || v_name)::regclass, 'SELECT') THEN
                RAISE EXCEPTION 'kernel contract v1: C10 failed: epigraph_maintenance cannot SELECT public.%', v_name;
            END IF;
        END LOOP;

        -- C11: the application role inserts claims, edges and events.
        FOREACH v_name IN ARRAY ARRAY['claims', 'edges', 'events'] LOOP
            v_rel := pg_catalog.to_regclass('public.' || v_name);
            IF v_rel IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: C11 failed: table public.% is missing', v_name;
            END IF;
            IF NOT pg_catalog.has_table_privilege('epigraph_app', v_rel, 'INSERT') THEN
                RAISE EXCEPTION 'kernel contract v1: C11 failed: epigraph_app cannot INSERT into public.%', v_name;
            END IF;
        END LOOP;

        -- C12: the viewer-resolve and parity-refusal functions.
        FOREACH v_name IN ARRAY ARRAY['epigraph_live_memberships', 'epigraph_operator_of_author'] LOOP
            v_proc := pg_catalog.to_regprocedure('public.' || v_name || '(uuid)');
            IF v_proc IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: C12 failed: function public.%(uuid) is missing', v_name;
            END IF;
            IF NOT pg_catalog.has_function_privilege('epigraph_app', v_proc, 'EXECUTE') THEN
                RAISE EXCEPTION 'kernel contract v1: C12 failed: epigraph_app cannot EXECUTE public.%(uuid)', v_name;
            END IF;
        END LOOP;

        -- C13: the application role reads the agent columns countersign
        -- verification and export need (the columns were checked above).
        FOREACH v_col IN ARRAY ARRAY['id', 'public_key', 'display_name'] LOOP
            IF NOT pg_catalog.has_column_privilege('epigraph_app', 'public.agents'::regclass, v_col, 'SELECT') THEN
                RAISE EXCEPTION 'kernel contract v1: C13 failed: epigraph_app cannot SELECT public.agents.%', v_col;
            END IF;
        END LOOP;

        -- C14: event publishing draws from the graph-version sequence.
        v_rel := pg_catalog.to_regclass('public.events_graph_version_seq');
        IF v_rel IS NULL THEN
            RAISE EXCEPTION 'kernel contract v1: C14 failed: sequence public.events_graph_version_seq is missing';
        END IF;
        IF NOT pg_catalog.has_sequence_privilege('epigraph_app', v_rel, 'USAGE') THEN
            RAISE EXCEPTION 'kernel contract v1: C14 failed: epigraph_app has no USAGE on public.events_graph_version_seq';
        END IF;

        -- L1: the legacy EpiScience head (5032, or the adopted hand-applied files).
        v_rel := pg_catalog.to_regclass('public.syntheses');
        IF v_rel IS NULL OR NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                                         WHERE a.attrelid = v_rel AND a.attname = 'autonomy_level'
                                           AND a.attnum > 0 AND NOT a.attisdropped) THEN
            RAISE EXCEPTION 'kernel contract v1: L1 failed: public.syntheses.autonomy_level is missing (the EpiScience legacy head)'
                USING HINT = 'Adopt or run 5032 first.';
        END IF;
    END;
    -- <<< contract v1 checks
END
$contract$;

-- The same checks as a callable assertion. Every EpiScience migration after
-- this one starts with `SELECT public.episcience_assert_kernel_contract(1);`.
-- SECURITY INVOKER: it reads the catalog and three kernel tables as its
-- caller, and grants nothing.
CREATE FUNCTION public.episcience_assert_kernel_contract(p_version integer)
RETURNS void
LANGUAGE plpgsql
STABLE
SECURITY INVOKER
SET search_path = public, pg_temp
AS $fn$
BEGIN
    IF p_version IS DISTINCT FROM 1 THEN
        RAISE EXCEPTION 'kernel contract: unknown contract version % (this database knows v1)',
            coalesce(p_version::text, 'NULL');
    END IF;
    -- >>> contract v1 checks
    -- The EpiGraph kernel objects EpiScience SQL may reference (tenancy
    -- contract v1, docs/tenancy-contract.md), plus the legacy EpiScience head
    -- (L1). Every item is present at kernel migration head 110. The first
    -- missing item RAISEs, naming it ("kernel contract v1: C<n> ..."), so a
    -- migration never half-applies on a kernel that drifted.
    --
    -- This region appears twice, verbatim: in the DO block that opens this
    -- migration (it runs before the function exists) and in
    -- public.episcience_assert_kernel_contract. A test compares the copies.
    -- Existence is checked before any privilege: the has_*_privilege
    -- functions ERROR on a missing role or column instead of returning false.
    DECLARE
        v_role     text;
        v_name     text;
        v_col      text;
        v_rel      regclass;
        v_proc     regprocedure;
        v_head     bigint;
        v_ext_nsp  text;
        v_spec     text[];
    BEGIN
        -- C1: the kernel roles.
        FOREACH v_role IN ARRAY ARRAY['epigraph_app', 'epigraph_maintenance'] LOOP
            IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = v_role) THEN
                RAISE EXCEPTION 'kernel contract v1: C1 failed: role % is missing', v_role
                    USING HINT = 'EpiScience needs the kernel tenancy roles (kernel migration head >= 110).';
            END IF;
        END LOOP;

        -- C2: the session functions, their result types, EXECUTE by epigraph_app.
        FOREACH v_spec SLICE 1 IN ARRAY ARRAY[
            ARRAY['epigraph_bypass',          'boolean'],
            ARRAY['epigraph_definer_bypass',  'boolean'],
            ARRAY['epigraph_session_groups',  'uuid[]'],
            ARRAY['epigraph_writable_groups', 'uuid[]'],
            ARRAY['epigraph_principal_id',    'uuid']
        ] LOOP
            v_proc := pg_catalog.to_regprocedure('public.' || v_spec[1] || '()');
            IF v_proc IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: C2 failed: function public.%() is missing', v_spec[1];
            END IF;
            IF pg_catalog.pg_get_function_result(v_proc) IS DISTINCT FROM v_spec[2] THEN
                RAISE EXCEPTION 'kernel contract v1: C2 failed: public.%() returns %, expected %',
                    v_spec[1], pg_catalog.pg_get_function_result(v_proc), v_spec[2];
            END IF;
            IF NOT pg_catalog.has_function_privilege('epigraph_app', v_proc, 'EXECUTE') THEN
                RAISE EXCEPTION 'kernel contract v1: C2 failed: epigraph_app cannot EXECUTE public.%()', v_spec[1];
            END IF;
        END LOOP;

        -- C3, C4, C5, C13: the kernel columns EpiScience reads or writes.
        FOREACH v_spec SLICE 1 IN ARRAY ARRAY[
            ARRAY['C3',  'groups',            'id'],
            ARRAY['C3',  'groups',            'kind'],
            ARRAY['C3',  'groups',            'did_key'],
            ARRAY['C3',  'groups',            'created_by_agent_id'],
            ARRAY['C3',  'group_memberships', 'group_id'],
            ARRAY['C3',  'group_memberships', 'agent_id'],
            ARRAY['C3',  'group_memberships', 'role'],
            ARRAY['C3',  'group_memberships', 'revoked_at'],
            ARRAY['C4',  'claims',            'id'],
            ARRAY['C4',  'claims',            'visibility'],
            ARRAY['C4',  'claims',            'owner_group_id'],
            ARRAY['C5',  'security_events',   'event_type'],
            ARRAY['C5',  'security_events',   'agent_id'],
            ARRAY['C5',  'security_events',   'success'],
            ARRAY['C5',  'security_events',   'details'],
            ARRAY['C13', 'agents',            'id'],
            ARRAY['C13', 'agents',            'public_key'],
            ARRAY['C13', 'agents',            'display_name']
        ] LOOP
            v_rel := pg_catalog.to_regclass('public.' || v_spec[2]);
            IF v_rel IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: % failed: table public.% is missing', v_spec[1], v_spec[2];
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                            WHERE a.attrelid = v_rel AND a.attname = v_spec[3]
                              AND a.attnum > 0 AND NOT a.attisdropped) THEN
                RAISE EXCEPTION 'kernel contract v1: % failed: column public.%.% is missing',
                    v_spec[1], v_spec[2], v_spec[3];
            END IF;
        END LOOP;

        -- C5: the maintenance role appends audit rows.
        IF NOT pg_catalog.has_table_privilege('epigraph_maintenance', 'public.security_events'::regclass, 'INSERT') THEN
            RAISE EXCEPTION 'kernel contract v1: C5 failed: epigraph_maintenance cannot INSERT into public.security_events';
        END IF;

        -- C6: the world and seed sentinel groups.
        IF NOT EXISTS (SELECT 1 FROM public.groups g
                        WHERE g.id = '00000000-0000-0000-0000-000000000000'::uuid AND g.kind = 'world') THEN
            RAISE EXCEPTION 'kernel contract v1: C6 failed: the world sentinel group is missing';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM public.groups g
                        WHERE g.id = '00000000-0000-0000-0000-00000000dead'::uuid AND g.kind = 'seed') THEN
            RAISE EXCEPTION 'kernel contract v1: C6 failed: the seed sentinel group is missing';
        END IF;

        -- C7: the kernel ledger (read-only) is at head 110 or later. The head
        -- is taken over kernel-range versions only (below 5000, the floor of
        -- EpiScience's range), so a foreign row cannot raise it, and version
        -- 110 itself must be recorded as applied, so an out-of-range row
        -- below the floor cannot stand in for it either.
        v_rel := pg_catalog.to_regclass('public._sqlx_migrations');
        IF v_rel IS NULL THEN
            RAISE EXCEPTION 'kernel contract v1: C7 failed: the kernel ledger public._sqlx_migrations is missing';
        END IF;
        SELECT max(m.version) INTO v_head FROM public._sqlx_migrations m
         WHERE m.success AND m.version < 5000;
        IF v_head IS NULL OR v_head < 110 THEN
            RAISE EXCEPTION 'kernel contract v1: C7 failed: kernel migration head is %, contract v1 needs >= 110',
                coalesce(v_head::text, 'none')
                USING HINT = 'Run the kernel''s epigraph-migrate first.';
        END IF;
        IF NOT EXISTS (SELECT 1 FROM public._sqlx_migrations m WHERE m.version = 110 AND m.success) THEN
            RAISE EXCEPTION 'kernel contract v1: C7 failed: kernel migration 110 is not recorded as applied'
                USING HINT = 'Run the kernel''s epigraph-migrate first.';
        END IF;

        -- C8: the entity-type registration of syntheses (read-only).
        IF pg_catalog.to_regclass('public.entity_types') IS NULL
           OR NOT EXISTS (SELECT 1 FROM public.entity_types e
                           WHERE e.type_name = 'synthesis' AND e.table_name = 'syntheses') THEN
            RAISE EXCEPTION 'kernel contract v1: C8 failed: public.entity_types has no synthesis -> syntheses row';
        END IF;

        -- C9: pgvector, in schema public (5032 qualifies its types as public.vector).
        SELECT n.nspname INTO v_ext_nsp
          FROM pg_catalog.pg_extension x
          JOIN pg_catalog.pg_namespace n ON n.oid = x.extnamespace
         WHERE x.extname = 'vector';
        IF v_ext_nsp IS DISTINCT FROM 'public' THEN
            RAISE EXCEPTION 'kernel contract v1: C9 failed: extension vector is in schema %, expected public',
                coalesce(v_ext_nsp, '(not installed)');
        END IF;

        -- C10: the maintenance role reads claims and groups.
        FOREACH v_name IN ARRAY ARRAY['claims', 'groups', 'group_memberships'] LOOP
            IF NOT pg_catalog.has_table_privilege('epigraph_maintenance', ('public.' || v_name)::regclass, 'SELECT') THEN
                RAISE EXCEPTION 'kernel contract v1: C10 failed: epigraph_maintenance cannot SELECT public.%', v_name;
            END IF;
        END LOOP;

        -- C11: the application role inserts claims, edges and events.
        FOREACH v_name IN ARRAY ARRAY['claims', 'edges', 'events'] LOOP
            v_rel := pg_catalog.to_regclass('public.' || v_name);
            IF v_rel IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: C11 failed: table public.% is missing', v_name;
            END IF;
            IF NOT pg_catalog.has_table_privilege('epigraph_app', v_rel, 'INSERT') THEN
                RAISE EXCEPTION 'kernel contract v1: C11 failed: epigraph_app cannot INSERT into public.%', v_name;
            END IF;
        END LOOP;

        -- C12: the viewer-resolve and parity-refusal functions.
        FOREACH v_name IN ARRAY ARRAY['epigraph_live_memberships', 'epigraph_operator_of_author'] LOOP
            v_proc := pg_catalog.to_regprocedure('public.' || v_name || '(uuid)');
            IF v_proc IS NULL THEN
                RAISE EXCEPTION 'kernel contract v1: C12 failed: function public.%(uuid) is missing', v_name;
            END IF;
            IF NOT pg_catalog.has_function_privilege('epigraph_app', v_proc, 'EXECUTE') THEN
                RAISE EXCEPTION 'kernel contract v1: C12 failed: epigraph_app cannot EXECUTE public.%(uuid)', v_name;
            END IF;
        END LOOP;

        -- C13: the application role reads the agent columns countersign
        -- verification and export need (the columns were checked above).
        FOREACH v_col IN ARRAY ARRAY['id', 'public_key', 'display_name'] LOOP
            IF NOT pg_catalog.has_column_privilege('epigraph_app', 'public.agents'::regclass, v_col, 'SELECT') THEN
                RAISE EXCEPTION 'kernel contract v1: C13 failed: epigraph_app cannot SELECT public.agents.%', v_col;
            END IF;
        END LOOP;

        -- C14: event publishing draws from the graph-version sequence.
        v_rel := pg_catalog.to_regclass('public.events_graph_version_seq');
        IF v_rel IS NULL THEN
            RAISE EXCEPTION 'kernel contract v1: C14 failed: sequence public.events_graph_version_seq is missing';
        END IF;
        IF NOT pg_catalog.has_sequence_privilege('epigraph_app', v_rel, 'USAGE') THEN
            RAISE EXCEPTION 'kernel contract v1: C14 failed: epigraph_app has no USAGE on public.events_graph_version_seq';
        END IF;

        -- L1: the legacy EpiScience head (5032, or the adopted hand-applied files).
        v_rel := pg_catalog.to_regclass('public.syntheses');
        IF v_rel IS NULL OR NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                                         WHERE a.attrelid = v_rel AND a.attname = 'autonomy_level'
                                           AND a.attnum > 0 AND NOT a.attisdropped) THEN
            RAISE EXCEPTION 'kernel contract v1: L1 failed: public.syntheses.autonomy_level is missing (the EpiScience legacy head)'
                USING HINT = 'Adopt or run 5032 first.';
        END IF;
    END;
    -- <<< contract v1 checks
END
$fn$;

-- True when the current session may write across owners: the current role is
-- a superuser or has BYPASSRLS, or the session holds the kernel's maintenance
-- bypass (epigraph_bypass: session_user) or runs inside a maintenance-owned
-- definer (epigraph_definer_bypass: current_user). EpiScience's row guards
-- (later migrations) exempt only such a session.
--
-- SECURITY INVOKER with the default EXECUTE grant. plpgsql with early RETURNs
-- on purpose: Postgres checks EXECUTE on every function of an expression when
-- the expression starts, so an OR of the three arms in one SQL expression
-- would demand EXECUTE on epigraph_definer_bypass (revoked from PUBLIC by the
-- kernel) even from a caller the first arm already admits. A caller that
-- reaches the last arm without EXECUTE on it gets an error, never a true.
-- It deliberately does not call any kernel function newer than head 110.
CREATE FUNCTION public.episcience_session_is_privileged()
RETURNS boolean
LANGUAGE plpgsql
STABLE
SECURITY INVOKER
SET search_path = public, pg_temp
AS $fn$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                WHERE r.rolname = current_user AND (r.rolsuper OR r.rolbypassrls)) THEN
        RETURN true;
    END IF;
    IF public.epigraph_bypass() THEN
        RETURN true;
    END IF;
    RETURN coalesce(public.epigraph_definer_bypass(), false);
END
$fn$;

-- The NOLOGIN roles EpiScience's grants and definers are issued to (the grants
-- themselves arrive with the RLS migrations). Roles are CLUSTER-scoped: create
-- each only when absent, tolerate a concurrent creator, and refuse a
-- pre-existing role of the same name that could log in, carries any elevated
-- attribute, or holds the kernel's maintenance role.
DO $roles$
DECLARE
    v_role text;
BEGIN
    FOREACH v_role IN ARRAY ARRAY['episcience_rw', 'episcience_queue', 'episcience_maint_ops'] LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = v_role) THEN
            BEGIN
                EXECUTE pg_catalog.format(
                    'CREATE ROLE %I NOLOGIN NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE NOREPLICATION INHERIT',
                    v_role);
            EXCEPTION WHEN duplicate_object OR unique_violation THEN
                NULL; -- created by a concurrent migration on another database of this cluster
            END;
        END IF;
        IF EXISTS (SELECT 1 FROM pg_catalog.pg_roles r
                    WHERE r.rolname = v_role
                      AND (r.rolcanlogin OR r.rolsuper OR r.rolbypassrls OR r.rolcreaterole
                           OR r.rolcreatedb OR r.rolreplication)) THEN
            RAISE EXCEPTION 'role % already exists with LOGIN or an elevated attribute; refusing to adopt it', v_role;
        END IF;
        IF pg_catalog.pg_has_role(v_role, 'epigraph_maintenance', 'MEMBER') THEN
            RAISE EXCEPTION 'role % is a member of epigraph_maintenance; refusing to adopt it', v_role;
        END IF;
    END LOOP;
END
$roles$;
