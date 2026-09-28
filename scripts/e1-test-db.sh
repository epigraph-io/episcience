#!/usr/bin/env bash
# scripts/e1-test-db.sh -- build the EpiScience test template on a Postgres TEST
# cluster and run a command against it.
#
#   E1_TEST_ADMIN_URL=postgres://<superuser>@127.0.0.1:5433/postgres \
#     scripts/e1-test-db.sh <batch> -- cargo test --workspace
#
# What it builds (every name ends in `_test` and starts with this run's unique
# prefix `episcience_e1_<batch>_<unix-ts>_<rand>`):
#   1. <prefix>_kernel_tmpl_test : the KERNEL schema, built by the kernel's own
#      `epigraph-migrate` at the rev EpiScience pins (derived from Cargo.lock).
#      Kept for the whole run: catalog-diff tests clone it.
#   2. <prefix>_tmpl_test        : a clone of (1) + `episcience-migrate run`,
#      then `epigraph-migrate` AGAIN (it must still exit 0: EpiScience never
#      writes the kernel ledger), then scripts/ci-roles.sql and
#      scripts/ci-seed.sql.
#   3. <prefix>_shared_test and <prefix>_eln_test : clones of (2) exported as
#      DATABASE_URL / EPISCIENCE_DATABASE_URL for the suites that share one DB.
# Per-test databases are cloned from (2) by the Rust harness
# (crates/episcience-db/tests/support/mod.rs, TestDb::fresh) under the same
# prefix. On EXIT every database carrying this run's prefix is dropped, and
# nothing else: the cluster is shared with other workflows.
#
# REFUSALS (no override): an admin URL on port 5432 (or with no port, which
# means 5432), an admin URL with a query string, any database name that does
# not end in `_test` or does not fit Postgres' 63-byte identifier limit.
#
# Secrets: DSNs are never printed. Tool logs are redacted before display.
#
# Optional environment:
#   E1_KERNEL_TOOLS_DIR    where epigraph-migrate is installed, one root per rev
#                          (default: $HOME/.cache/episcience-e1)
#   E1_CARGO_INSTALL_FLAGS extra `cargo install` flags (e.g. --debug locally)
set -euo pipefail

die() { echo "[e1-test-db] $*" >&2; exit 2; }
redact() { sed -E 's#postgres(ql)?://[^[:space:]]*#postgres://<redacted>#g'; }

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

batch=${1:-}
[ -n "$batch" ] || die "usage: $0 <batch> -- <command...>"
shift
[ "${1:-}" = "--" ] && shift
[ $# -gt 0 ] || die "usage: $0 <batch> -- <command...> (a command is required)"
[[ "$batch" =~ ^[a-z0-9]{1,12}$ ]] || die "REFUSED: batch must be 1-12 of [a-z0-9]"

: "${E1_TEST_ADMIN_URL:?E1_TEST_ADMIN_URL must name the admin database of the TEST cluster}"

# ---- the admin URL: never port 5432 ------------------------------------------
case "$E1_TEST_ADMIN_URL" in
  postgres://*|postgresql://*) ;;
  *) die "REFUSED: E1_TEST_ADMIN_URL is not a postgres:// URL" ;;
esac
case "$E1_TEST_ADMIN_URL" in *\?*) die "REFUSED: E1_TEST_ADMIN_URL must not carry a query string" ;; esac
rest=${E1_TEST_ADMIN_URL#*://}
rest=${rest##*@}
hostport=${rest%%/*}
port=${hostport##*:}
[ "$port" = "$hostport" ] && port=5432
[[ "$port" =~ ^[0-9]+$ ]] || die "REFUSED: cannot read the port of E1_TEST_ADMIN_URL"
[ "$port" != "5432" ] || die "REFUSED: port 5432 (the test cluster is never on 5432)"
[[ "$rest" == */* ]] || die "REFUSED: E1_TEST_ADMIN_URL names no database"
BASE=${E1_TEST_ADMIN_URL%/*}

check_name() {
  local n=$1
  [[ "$n" =~ ^[a-z0-9_]+$ ]] || die "REFUSED: database name has characters outside [a-z0-9_]"
  [ "${#n}" -le 63 ] || die "REFUSED: database name longer than 63 bytes (Postgres would truncate it)"
  [[ "$n" == *_test ]] || die "REFUSED: database name does not end in _test"
}

psql_admin() { psql "$E1_TEST_ADMIN_URL" -X -q -v ON_ERROR_STOP=1 "$@"; }

# ---- the pinned kernel rev ---------------------------------------------------
revs=$(grep -o 'git+https://github.com/epigraph-io/epigraph?rev=[0-9a-f]\{40\}' "$REPO/Cargo.lock" | sed 's/.*rev=//' | sort -u)
[ -n "$revs" ] || die "no epigraph-io/epigraph git rev in Cargo.lock"
[ "$(printf '%s\n' "$revs" | wc -l)" -eq 1 ] || die "Cargo.lock pins more than one kernel rev"
KREV=$revs

TOOLS=${E1_KERNEL_TOOLS_DIR:-$HOME/.cache/episcience-e1}
MIGRATE_ROOT="$TOOLS/epigraph-migrate-$KREV"
MIGRATE_BIN="$MIGRATE_ROOT/bin/epigraph-migrate"
if [ ! -x "$MIGRATE_BIN" ]; then
  echo "[e1-test-db] installing epigraph-migrate at kernel rev ${KREV:0:12}"
  # shellcheck disable=SC2086
  SQLX_OFFLINE=true cargo install --locked \
    --git https://github.com/epigraph-io/epigraph --rev "$KREV" \
    epigraph-api --bin epigraph-migrate \
    --root "$MIGRATE_ROOT" --target-dir "$TOOLS/target" ${E1_CARGO_INSTALL_FLAGS:-}
fi

# ---- names -------------------------------------------------------------------
TS=$(date +%s)
RND=$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')
PREFIX="episcience_e1_${batch}_${TS}_${RND}"
KT="${PREFIX}_kernel_tmpl_test"
T="${PREFIX}_tmpl_test"
SHARED="${PREFIX}_shared_test"
ELN="${PREFIX}_eln_test"
for n in "$KT" "$T" "$SHARED" "$ELN" "${PREFIX}_0123abcd_test"; do check_name "$n"; done

LOGDIR=$(mktemp -d)
cleanup() {
  local rc=$?
  local dbs
  dbs=$(psql "$E1_TEST_ADMIN_URL" -X -tA -c \
    "SELECT datname FROM pg_database WHERE left(datname, length('${PREFIX}_')) = '${PREFIX}_'" 2>/dev/null || true)
  for d in $dbs; do
    case "$d" in "${PREFIX}"_*_test)
      psql "$E1_TEST_ADMIN_URL" -X -q -c "DROP DATABASE IF EXISTS \"$d\" WITH (FORCE);" >/dev/null 2>&1 || true ;;
    esac
  done
  rm -rf "$LOGDIR"
  echo "[e1-test-db] dropped every database of run ${PREFIX}"
  exit "$rc"
}
trap cleanup EXIT

run_logged() {
  local what=$1; shift
  if ! "$@" >"$LOGDIR/$what.log" 2>&1; then
    echo "[e1-test-db] FAILED: $what" >&2
    tail -40 "$LOGDIR/$what.log" | redact >&2
    exit 3
  fi
}

# ---- 1. kernel-only template ---------------------------------------------------
psql_admin -c "CREATE DATABASE \"$KT\";"
run_logged kernel-migrate env -u DATABASE_URL MIGRATION_DATABASE_URL="$BASE/$KT" "$MIGRATE_BIN"
echo "[e1-test-db] kernel schema built by epigraph-migrate (rev ${KREV:0:12})"

# ---- 2. EpiScience template ------------------------------------------------------
psql_admin -c "CREATE DATABASE \"$T\" TEMPLATE \"$KT\";"
( cd "$REPO" && cargo build -q --locked -p episcience-api --bin episcience-migrate )
MIG_TARGET=${CARGO_TARGET_DIR:-$REPO/target}
run_logged episcience-migrate env -u DATABASE_URL EPISCIENCE_MIGRATION_DATABASE_URL="$BASE/$T" \
  "$MIG_TARGET/debug/episcience-migrate" run
# The kernel migrator must still accept the database: EpiScience wrote no
# version into public._sqlx_migrations (ledger isolation).
run_logged kernel-migrate-again env -u DATABASE_URL MIGRATION_DATABASE_URL="$BASE/$T" "$MIGRATE_BIN"
psql "$BASE/$T" -X -q -v ON_ERROR_STOP=1 -f "$REPO/scripts/ci-roles.sql" >/dev/null
psql "$BASE/$T" -X -q -v ON_ERROR_STOP=1 -f "$REPO/scripts/ci-seed.sql" >/dev/null
echo "[e1-test-db] EpiScience template ready (episcience-migrate run; kernel migrator re-run exits 0)"

# ---- 3. no session may stay on a template ------------------------------------------
psql_admin -tA -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname IN ('$KT', '$T') AND pid <> pg_backend_pid();" >/dev/null
psql_admin -c "CREATE DATABASE \"$SHARED\" TEMPLATE \"$T\";"
psql_admin -c "CREATE DATABASE \"$ELN\" TEMPLATE \"$T\";"

export E1_TEST_ADMIN_URL
export E1_RUN_PREFIX="$PREFIX"
export E1_TEMPLATE_DB="$T"
export E1_KERNEL_TEMPLATE_DB="$KT"
export E1_KERNEL_REV="$KREV"
export E1_EPIGRAPH_MIGRATE_BIN="$MIGRATE_BIN"
export DATABASE_URL="$BASE/$SHARED"
export EPISCIENCE_DATABASE_URL="$BASE/$ELN"

set +e
"$@"
rc=$?
set -e
exit $rc
