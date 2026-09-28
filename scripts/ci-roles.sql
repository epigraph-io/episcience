-- scripts/ci-roles.sql -- CI/test-cluster LOGIN roles for EpiScience's
-- processes (brief logins: episcience_app, episcience_worker, episcience_maint).
--
-- TEST CLUSTERS ONLY. The passwords below are CI-only literals; production
-- logins are created out of band with generated passwords, never by a script
-- in this repository and never by a migration.
--
-- Rules this file keeps:
--   * it refuses to run unless current_database() ends in `_test`;
--   * roles are CLUSTER-scoped and other workflows share the cluster, so it
--     only CREATEs a role that is absent and never ALTERs any existing role's
--     attributes or password (epigraph_app, epigraph_maintenance and
--     epigraph_seed above all);
--   * memberships are granted only in roles that exist: the EpiScience NOLOGIN
--     roles (episcience_rw, episcience_queue, episcience_maint_ops) arrive with
--     a later migration, and this file is re-run on every template build.
DO $$
DECLARE
    r record;
BEGIN
    IF right(current_database(), 5) <> '_test' THEN
        RAISE EXCEPTION 'ci-roles.sql refuses database %: the name must end in _test',
            current_database();
    END IF;

    FOR r IN
        SELECT * FROM (VALUES
            ('episcience_app',    'episcience_app_ci_only'),
            ('episcience_worker', 'episcience_worker_ci_only'),
            ('episcience_maint',  'episcience_maint_ci_only')
        ) AS v(rolname, pw)
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r.rolname) THEN
            EXECUTE format(
                'CREATE ROLE %I LOGIN NOSUPERUSER NOBYPASSRLS NOCREATEROLE NOCREATEDB INHERIT PASSWORD %L',
                r.rolname, r.pw);
        END IF;
    END LOOP;

    FOR r IN
        SELECT * FROM (VALUES
            ('episcience_app',    'epigraph_app'),
            ('episcience_app',    'episcience_rw'),
            ('episcience_worker', 'epigraph_app'),
            ('episcience_worker', 'episcience_rw'),
            ('episcience_worker', 'episcience_queue'),
            ('episcience_maint',  'episcience_maint_ops')
        ) AS v(member, grp)
    LOOP
        IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = r.grp)
           AND NOT pg_has_role(r.member, r.grp, 'MEMBER') THEN
            EXECUTE format('GRANT %I TO %I', r.grp, r.member);
        END IF;
    END LOOP;
END $$;
