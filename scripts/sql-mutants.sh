#!/usr/bin/env bash
# scripts/sql-mutants.sh -- the SQL half of the batch mutation pass (brief 6.4).
#
#   scripts/e1-test-db.sh <batch> -- scripts/sql-mutants.sh
#
# Runs UNDER scripts/e1-test-db.sh (which exports E1_TEST_ADMIN_URL,
# E1_TEMPLATE_DB and E1_RUN_PREFIX). For each mutation below it clones the
# template, applies the mutation to the clone, points the test harness at the
# MUTATED clone (E1_TEMPLATE_DB), runs the guard tests, and requires at least
# one of them to FAIL. A mutation the tests do not notice ("SURVIVED") makes
# the script exit non-zero. Every clone carries this run's prefix, so the
# wrapper's EXIT trap drops it whatever happens; the script drops each one as
# it goes too. DSNs are never printed.
#
# Mutations (the ones that exist once the tenancy columns are in: the policy
# and grant mutations of the list join when row security does):
#   drop one trigger (each guard kind), make a guard function DEFINER,
#   remove the principal_id forcing, drop the real-group CHECK, remove the
#   widening interlock, remove the claim guard's owner comparison, make the
#   inherit honour a declaration, weaken publishability, let a refinement
#   name any group, and each child block of the propagation made a no-op
#   (its count check compares a table with itself, so only the tests see it).
#   Review round: every row guard on every table it is installed on (author,
#   owner, derived pin, claim guard, widening guard, parent pin), each arm of
#   both publishability helpers, the insert-time and attach-time narrowing,
#   the publish rule on any status change, the declared-pair arms of blobs and
#   sample links, and the child-sample filter and orphan refusal of the
#   propagation.
#   A no-mutation control runs first and must pass.
#   The data steps of 5034/5035 run from the migration FILES (not the
#   template), so their mutants are applied to the source and rebuilt; that
#   pass is scripted per batch outside this file (see the E1d runbook).
set -euo pipefail

: "${E1_TEST_ADMIN_URL:?run under scripts/e1-test-db.sh}"
: "${E1_TEMPLATE_DB:?run under scripts/e1-test-db.sh}"
: "${E1_RUN_PREFIX:?run under scripts/e1-test-db.sh}"
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BASE=${E1_TEST_ADMIN_URL%/*}
TESTS=${SQL_MUTANTS_CARGO_ARGS:-"-p episcience-db --test tenancy_guards_test"}
# The two tests that run docs/runbooks/5035-undo.sql fail on ANY dropped
# trigger (its DROP TRIGGER errors), which is a structural failure, not a
# behavioural one: they are skipped so that a mutant counts as killed only
# when a behavioural test notices it.
SKIPS="--skip the_5035_undo_script_reverts_and_5035_reapplies --skip the_rollback_leaves_values_the_previous_binary_decodes"

# name | SQL applied to the clone (one line).
MUTANTS=$(cat <<'EOF'
drop the author trigger on syntheses|DROP TRIGGER tenancy_15_author ON public.syntheses;
drop the inherit trigger on synthesis_clusters|DROP TRIGGER tenancy_10_inherit ON public.synthesis_clusters;
drop the claim guard on synthesis_claim_membership|DROP TRIGGER tenancy_20_claim_guard ON public.synthesis_claim_membership;
drop the job principal trigger|DROP TRIGGER tenancy_20_principal ON public.synthesis_jobs;
drop the owner-immutable trigger on syntheses|DROP TRIGGER tenancy_30_owner_immutable ON public.syntheses;
drop the derived pin on synthesis_clusters|DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_clusters;
drop the widening guard on syntheses|DROP TRIGGER tenancy_40_widening_guard ON public.syntheses;
drop the publish rule|DROP TRIGGER tenancy_45_publish_rule ON public.syntheses;
drop the propagation on syntheses|DROP TRIGGER tenancy_90_propagate ON public.syntheses;
drop the root require trigger on syntheses|DROP TRIGGER tenancy_10_require ON public.syntheses;
drop the root require trigger on protocols|DROP TRIGGER tenancy_10_require ON public.protocols;
drop the countersignature claim guard|DROP TRIGGER tenancy_20_claim_guard ON public.countersignatures;
make the author guard a DEFINER owned by maintenance|ALTER FUNCTION public.episcience_author_is_principal() SECURITY DEFINER; ALTER FUNCTION public.episcience_author_is_principal() OWNER TO epigraph_maintenance;
make the job principal guard a DEFINER owned by maintenance|ALTER FUNCTION public.episcience_job_principal() SECURITY DEFINER; ALTER FUNCTION public.episcience_job_principal() OWNER TO epigraph_maintenance;
drop the real-group CHECK on syntheses|ALTER TABLE public.syntheses DROP CONSTRAINT syntheses_group_needs_real_group;
remove the principal_id forcing|CREATE OR REPLACE FUNCTION public.episcience_job_principal() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ BEGIN RETURN NEW; END $f$;
remove the widening interlock|CREATE OR REPLACE FUNCTION public.episcience_block_widening() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ BEGIN IF TG_TABLE_NAME = 'syntheses' AND NOT public.episcience_synthesis_is_publishable(NEW.id, NEW.parent_synthesis_id, NEW.prereq_synthesis_ids) THEN RAISE EXCEPTION 'x' USING ERRCODE = '42501'; END IF; RETURN NEW; END $f$;
publishability ignores member claims|CREATE OR REPLACE FUNCTION public.episcience_synthesis_is_publishable(p_id uuid, p_parent uuid, p_prereqs uuid[]) RETURNS boolean LANGUAGE sql STABLE SET search_path = public, pg_temp AS $f$ SELECT (p_parent IS NULL OR EXISTS (SELECT 1 FROM syntheses p WHERE p.id = p_parent AND p.visibility = 'public')) AND NOT EXISTS (SELECT 1 FROM unnest(coalesce(p_prereqs, ARRAY[]::uuid[])) x(id) LEFT JOIN syntheses p ON p.id = x.id WHERE p.id IS NULL OR p.visibility <> 'public') $f$;
publishability ignores prerequisites|CREATE OR REPLACE FUNCTION public.episcience_synthesis_is_publishable(p_id uuid, p_parent uuid, p_prereqs uuid[]) RETURNS boolean LANGUAGE sql STABLE SET search_path = public, pg_temp AS $f$ SELECT NOT EXISTS (SELECT 1 FROM synthesis_claim_membership m LEFT JOIN claims c ON c.id = m.claim_id WHERE m.synthesis_id = p_id AND (c.id IS NULL OR c.visibility::text <> 'public')) AND (p_parent IS NULL OR EXISTS (SELECT 1 FROM syntheses p WHERE p.id = p_parent AND p.visibility = 'public')) $f$;
the claim guard ignores the owner|CREATE OR REPLACE FUNCTION public.episcience_claim_attach_guard() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ BEGIN PERFORM 1 FROM claims c WHERE c.id = NEW.claim_id; IF NOT FOUND THEN RAISE EXCEPTION 'x' USING ERRCODE = '23503'; END IF; RETURN NEW; END $f$;
the inherit honours a declaration|CREATE OR REPLACE FUNCTION public.episcience_inherit_from_synthesis() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ DECLARE v_owner uuid; v_vis text; BEGIN SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis FROM syntheses p WHERE p.id = (to_jsonb(NEW) ->> TG_ARGV[0])::uuid; IF NOT FOUND THEN RAISE EXCEPTION 'x' USING ERRCODE = '23503'; END IF; NEW.owner_group_id := coalesce(NEW.owner_group_id, v_owner); NEW.visibility := coalesce(NEW.visibility, v_vis); RETURN NEW; END $f$;
the propagation skips synthesis_clusters|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE synthesis_clusters x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE synthesis_clusters x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips synthesis_embeddings|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE synthesis_embeddings x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE synthesis_embeddings x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips synthesis_staleness_events|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE synthesis_staleness_events x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE synthesis_staleness_events x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips synthesis_provo_edges|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE synthesis_provo_edges x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE synthesis_provo_edges x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips synthesis_claim_membership|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE synthesis_claim_membership x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE synthesis_claim_membership x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips synthesis_jobs|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE synthesis_jobs x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE synthesis_jobs x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips sample_claims|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE sample_claims x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE sample_claims x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips blobs|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE blobs x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE blobs x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation skips child samples|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'UPDATE samples x SET owner_group_id = c.owner_group_id, visibility = c.visibility', 'UPDATE samples x SET owner_group_id = x.owner_group_id, visibility = x.visibility'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
a refinement may name any group|CREATE OR REPLACE FUNCTION public.episcience_root_require_tenancy() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ BEGIN IF NEW.owner_group_id IS NULL OR NEW.visibility IS NULL THEN RAISE EXCEPTION 'x' USING ERRCODE = '23502'; END IF; RETURN NEW; END $f$;
drop the widening guard on samples|DROP TRIGGER tenancy_40_widening_guard ON public.samples;
drop the author trigger on samples|DROP TRIGGER tenancy_15_author ON public.samples;
drop the author trigger on protocols|DROP TRIGGER tenancy_15_author ON public.protocols;
drop the author trigger on blobs|DROP TRIGGER tenancy_15_author ON public.blobs;
drop the author trigger on countersignatures|DROP TRIGGER tenancy_15_author ON public.countersignatures;
drop the claim guard on sample_claims|DROP TRIGGER tenancy_20_claim_guard ON public.sample_claims;
drop the owner-immutable trigger on samples|DROP TRIGGER tenancy_30_owner_immutable ON public.samples;
drop the owner-immutable trigger on protocols|DROP TRIGGER tenancy_30_owner_immutable ON public.protocols;
drop the owner-immutable trigger on blobs|DROP TRIGGER tenancy_30_owner_immutable ON public.blobs;
drop the owner-immutable trigger on countersignatures|DROP TRIGGER tenancy_30_owner_immutable ON public.countersignatures;
drop the derived pin on sample_claims|DROP TRIGGER tenancy_30_derived_pinned ON public.sample_claims;
drop the derived pin on synthesis_jobs|DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_jobs;
drop the derived pin on blobs|DROP TRIGGER tenancy_30_derived_pinned ON public.blobs;
drop the derived pin on synthesis_claim_membership|DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_claim_membership;
drop the derived pin on synthesis_embeddings|DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_embeddings;
drop the derived pin on synthesis_staleness_events|DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_staleness_events;
drop the derived pin on synthesis_provo_edges|DROP TRIGGER tenancy_30_derived_pinned ON public.synthesis_provo_edges;
drop the parent pin on syntheses|DROP TRIGGER tenancy_12_parent_pinned ON public.syntheses;
drop the parent pin on synthesis_clusters|DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_clusters;
drop the parent pin on synthesis_embeddings|DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_embeddings;
drop the parent pin on synthesis_staleness_events|DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_staleness_events;
drop the parent pin on synthesis_provo_edges|DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_provo_edges;
drop the parent pin on synthesis_claim_membership|DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_claim_membership;
drop the parent pin on synthesis_jobs|DROP TRIGGER tenancy_12_parent_pinned ON public.synthesis_jobs;
drop the parent pin on samples|DROP TRIGGER tenancy_12_parent_pinned ON public.samples;
drop the parent pin on sample_claims|DROP TRIGGER tenancy_12_parent_pinned ON public.sample_claims;
drop the parent pin on protocols|DROP TRIGGER tenancy_12_parent_pinned ON public.protocols;
drop the parent pin on blobs|DROP TRIGGER tenancy_12_parent_pinned ON public.blobs;
drop the parent pin on countersignatures|DROP TRIGGER tenancy_12_parent_pinned ON public.countersignatures;
drop the narrowing on a non-public member|DROP TRIGGER tenancy_50_narrow_parent ON public.synthesis_claim_membership;
the publish rule fires only on completion|DROP TRIGGER tenancy_45_publish_rule ON public.syntheses; CREATE TRIGGER tenancy_45_publish_rule BEFORE UPDATE OF status ON public.syntheses FOR EACH ROW WHEN (NEW.status = 'complete' AND OLD.status IS DISTINCT FROM 'complete' AND NEW.visibility = 'public') EXECUTE FUNCTION public.episcience_publish_rule();
synthesis publishability ignores the parent|CREATE OR REPLACE FUNCTION public.episcience_synthesis_is_publishable(p_id uuid, p_parent uuid, p_prereqs uuid[]) RETURNS boolean LANGUAGE sql STABLE SET search_path = public, pg_temp AS $f$ SELECT NOT EXISTS (SELECT 1 FROM synthesis_claim_membership m LEFT JOIN claims c ON c.id = m.claim_id WHERE m.synthesis_id = p_id AND (c.id IS NULL OR c.visibility::text <> 'public')) AND NOT EXISTS (SELECT 1 FROM unnest(coalesce(p_prereqs, ARRAY[]::uuid[])) x(id) LEFT JOIN syntheses p ON p.id = x.id WHERE p.id IS NULL OR p.visibility <> 'public') $f$;
sample publishability ignores its claims|CREATE OR REPLACE FUNCTION public.episcience_sample_is_publishable(p_id uuid, p_parent uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = public, pg_temp AS $f$ SELECT (p_parent IS NULL OR EXISTS (SELECT 1 FROM samples p WHERE p.id = p_parent AND p.visibility = 'public')) $f$;
sample publishability ignores its parent|CREATE OR REPLACE FUNCTION public.episcience_sample_is_publishable(p_id uuid, p_parent uuid) RETURNS boolean LANGUAGE sql STABLE SET search_path = public, pg_temp AS $f$ SELECT NOT EXISTS (SELECT 1 FROM sample_claims sc LEFT JOIN claims c ON c.id = sc.claim_id WHERE sc.sample_id = p_id AND (c.id IS NULL OR c.visibility::text <> 'public')) $f$;
the insert-time narrowing is removed|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_root_require_tenancy()'::regprocedure) INTO d; m := replace(d, 'NEW.visibility := ''group'';', 'NULL;'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
a blob on a sample honours its declared pair|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_root_require_tenancy()'::regprocedure) INTO d; m := replace(d, E'NEW.owner_group_id := v_owner;\n            NEW.visibility := v_vis;\n            RETURN NEW;', E'NEW.owner_group_id := coalesce(NEW.owner_group_id, v_owner);\n            NEW.visibility := coalesce(NEW.visibility, v_vis);\n            RETURN NEW;'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the sample inherit honours a declaration|CREATE OR REPLACE FUNCTION public.episcience_inherit_from_sample() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ DECLARE v_owner uuid; v_vis text; BEGIN SELECT p.owner_group_id, p.visibility INTO v_owner, v_vis FROM samples p WHERE p.id = (to_jsonb(NEW) ->> TG_ARGV[0])::uuid; IF NOT FOUND THEN RAISE EXCEPTION 'x' USING ERRCODE = '23503'; END IF; NEW.owner_group_id := coalesce(NEW.owner_group_id, v_owner); NEW.visibility := coalesce(NEW.visibility, v_vis); RETURN NEW; END $f$;
the claim guard admits a group claim on a public sample|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_claim_attach_guard()'::regprocedure) INTO d; m := replace(d, 'RAISE EXCEPTION ''a public sample attaches public claims only''', 'RAISE NOTICE ''a public sample attaches public claims only'''); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation re-owns every child sample|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'AND (x.owner_group_id, x.visibility) IS NOT DISTINCT FROM (p.owner_group_id, p.visibility)', 'AND (x.owner_group_id, x.visibility) IS DISTINCT FROM (c.owner_group_id, c.visibility)'); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
the propagation strands another owner's child under a group sample|DO $m$ DECLARE d text; m text; BEGIN SELECT pg_get_functiondef('public.episcience_propagate_parent_tenancy()'::regprocedure) INTO d; m := replace(d, 'RAISE EXCEPTION ''a child sample owned by another group would sit under a group sample''', 'RAISE NOTICE ''a child sample owned by another group would sit under a group sample'''); IF m = d THEN RAISE EXCEPTION 'mutation not applied'; END IF; EXECUTE m; END $m$;
EOF
)

# Control: an unmutated clone must PASS, or every "killed" below would be
# vacuous (a broken clone fails every test for the wrong reason).
ctl="${E1_RUN_PREFIX}_mut0_test"
psql "$E1_TEST_ADMIN_URL" -X -q -v ON_ERROR_STOP=1 -c "CREATE DATABASE \"$ctl\" TEMPLATE \"$E1_TEMPLATE_DB\";" >/dev/null
if ( cd "$REPO" && E1_TEMPLATE_DB="$ctl" cargo test -q $TESTS -- --test-threads=4 $SKIPS >/dev/null 2>&1 ); then
  echo "[sql-mutants] control (no mutation): passes"
else
  echo "[sql-mutants] control (no mutation) FAILED: the mutation results would be meaningless" >&2
  psql "$E1_TEST_ADMIN_URL" -X -q -c "DROP DATABASE IF EXISTS \"$ctl\" WITH (FORCE);" >/dev/null 2>&1 || true
  exit 3
fi
psql "$E1_TEST_ADMIN_URL" -X -q -c "DROP DATABASE IF EXISTS \"$ctl\" WITH (FORCE);" >/dev/null 2>&1 || true

survived=0
killed=0
i=0
while IFS='|' read -r name sql; do
  [ -n "$name" ] || continue
  i=$((i + 1))
  # SQL_MUTANTS_RANGE=<first>-<last> runs a slice (1-based, inclusive), so a
  # long pass can be split into foreground runs; the control always runs.
  if [ -n "${SQL_MUTANTS_RANGE:-}" ]; then
    lo=${SQL_MUTANTS_RANGE%-*}; hi=${SQL_MUTANTS_RANGE#*-}
    { [ "$i" -ge "$lo" ] && [ "$i" -le "$hi" ]; } || continue
  fi
  db="${E1_RUN_PREFIX}_mut${i}_test"
  psql "$E1_TEST_ADMIN_URL" -X -q -v ON_ERROR_STOP=1 -c "CREATE DATABASE \"$db\" TEMPLATE \"$E1_TEMPLATE_DB\";" >/dev/null
  if ! psql "$BASE/$db" -X -q -v ON_ERROR_STOP=1 -c "$sql" >/dev/null 2>&1; then
    echo "[sql-mutants] M$i $name: MUTATION NOT APPLIED"
    survived=$((survived + 1))
  elif ( cd "$REPO" && E1_TEMPLATE_DB="$db" cargo test -q $TESTS -- --test-threads=4 $SKIPS >/dev/null 2>&1 ); then
    echo "[sql-mutants] M$i $name: SURVIVED"
    survived=$((survived + 1))
  else
    echo "[sql-mutants] M$i $name: killed"
    killed=$((killed + 1))
  fi
  psql "$E1_TEST_ADMIN_URL" -X -q -c "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE);" >/dev/null 2>&1 || true
done <<< "$MUTANTS"

echo "[sql-mutants] killed $killed, survived or not applied $survived"
[ "$survived" -eq 0 ]
