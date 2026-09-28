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
#   name any group. A no-mutation control runs first and must pass.
set -euo pipefail

: "${E1_TEST_ADMIN_URL:?run under scripts/e1-test-db.sh}"
: "${E1_TEMPLATE_DB:?run under scripts/e1-test-db.sh}"
: "${E1_RUN_PREFIX:?run under scripts/e1-test-db.sh}"
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BASE=${E1_TEST_ADMIN_URL%/*}
TESTS=${SQL_MUTANTS_CARGO_ARGS:-"-p episcience-db --test tenancy_guards_test"}

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
a refinement may name any group|CREATE OR REPLACE FUNCTION public.episcience_root_require_tenancy() RETURNS trigger LANGUAGE plpgsql SET search_path = public, pg_temp AS $f$ BEGIN IF NEW.owner_group_id IS NULL OR NEW.visibility IS NULL THEN RAISE EXCEPTION 'x' USING ERRCODE = '23502'; END IF; RETURN NEW; END $f$;
EOF
)

# Control: an unmutated clone must PASS, or every "killed" below would be
# vacuous (a broken clone fails every test for the wrong reason).
ctl="${E1_RUN_PREFIX}_mut0_test"
psql "$E1_TEST_ADMIN_URL" -X -q -v ON_ERROR_STOP=1 -c "CREATE DATABASE \"$ctl\" TEMPLATE \"$E1_TEMPLATE_DB\";" >/dev/null
if ( cd "$REPO" && E1_TEMPLATE_DB="$ctl" cargo test -q $TESTS -- --test-threads=4 >/dev/null 2>&1 ); then
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
  db="${E1_RUN_PREFIX}_mut${i}_test"
  psql "$E1_TEST_ADMIN_URL" -X -q -v ON_ERROR_STOP=1 -c "CREATE DATABASE \"$db\" TEMPLATE \"$E1_TEMPLATE_DB\";" >/dev/null
  if ! psql "$BASE/$db" -X -q -v ON_ERROR_STOP=1 -c "$sql" >/dev/null 2>&1; then
    echo "[sql-mutants] M$i $name: MUTATION NOT APPLIED"
    survived=$((survived + 1))
  elif ( cd "$REPO" && E1_TEMPLATE_DB="$db" cargo test -q $TESTS -- --test-threads=4 >/dev/null 2>&1 ); then
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
