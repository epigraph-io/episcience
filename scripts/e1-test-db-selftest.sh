#!/usr/bin/env bash
# scripts/e1-test-db-selftest.sh -- the admin-URL refusals of scripts/e1-test-db.sh,
# checked WITHOUT any database: every case below is refused before the script
# runs psql, cargo or anything else, and one accepted URL (with
# E1_TEST_DB_CHECK_ONLY=1, which stops right after the checks with exit code 4)
# proves the refusals come from the URL and not from an unrelated early exit.
# The last case proves check-only mode never runs the command and never exits 0.
# No credentials: the URLs name no real cluster.
set -uo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/e1-test-db.sh"
fail=0

expect_refused() {
  local why=$1 url=$2 out rc
  out=$(E1_TEST_DB_CHECK_ONLY=1 E1_TEST_ADMIN_URL="$url" "$SCRIPT" selftest -- true 2>&1)
  rc=$?
  if [ "$rc" -eq 2 ] && [[ "$out" == *REFUSED* ]]; then
    echo "ok   refused: $why"
  else
    echo "FAIL not refused ($rc): $why"; fail=1
  fi
}

expect_accepted() {
  local why=$1 url=$2 out rc
  out=$(E1_TEST_DB_CHECK_ONLY=1 E1_TEST_ADMIN_URL="$url" "$SCRIPT" selftest -- true 2>&1)
  rc=$?
  # Check-only mode exits 4 (never 0) and must not run the command.
  if [ "$rc" -eq 4 ] && [[ "$out" == *"admin URL accepted (check only"* ]]; then
    echo "ok   accepted: $why"
  else
    echo "FAIL not accepted ($rc): $why"; fail=1
  fi
}

expect_refused "port 5432"                       "postgres://u:p@127.0.0.1:5432/postgres"
expect_refused "no port (libpq default 5432)"    "postgres://u:p@127.0.0.1/postgres"
expect_refused "no host and no port"             "postgres://u:p@/postgres"
expect_refused "multi-host, 5432 first"          "postgres://u:p@127.0.0.1:5432,127.0.0.1:5433/postgres"
expect_refused "multi-host, 5433 first"          "postgres://u:p@127.0.0.1:5433,127.0.0.1:5432/postgres"
expect_refused "query string"                    "postgres://u:p@127.0.0.1:5433/postgres?port=5432"
expect_refused "port= outside a query string"    "postgres://u:p@127.0.0.1:5433/postgres&port=5432"
expect_refused "percent-encoded host"            "postgres://u:p@127.0.0.1%2C127.0.0.1:5433/postgres"
expect_refused "not a postgres URL"              "mysql://u:p@127.0.0.1:5433/postgres"
expect_refused "no database"                     "postgres://u:p@127.0.0.1:5433"
expect_refused "empty database name"             "postgres://u:p@127.0.0.1:5433/"
# libpq reads the host after the FIRST '@' before the first '/': here it
# connects to 127.0.0.1:5432 with dbname 'db@x:5433/postgres'.
expect_refused "'@' in the database path"        "postgres://u@127.0.0.1:5432/db@x:5433/postgres"
expect_refused "'@' in the path, port 5433"      "postgres://u@127.0.0.1:5433/db@x"
expect_refused "second '@' in the authority"     "postgres://u@127.0.0.1:5432@x:5433/postgres"
expect_refused "'/' inside the database name"    "postgres://u@127.0.0.1:5433/a/postgres"
expect_accepted "single host on 5433"            "postgres://u:p@127.0.0.1:5433/postgres"
expect_accepted "postgresql:// scheme on 5433"   "postgresql://u@localhost:5433/postgres"

# The batch name is folded into every database name; a bad one is refused too.
out=$(E1_TEST_DB_CHECK_ONLY=1 E1_TEST_ADMIN_URL="postgres://u@127.0.0.1:5433/postgres" "$SCRIPT" 'Bad-Name' -- true 2>&1)
if [ $? -eq 2 ] && [[ "$out" == *REFUSED* ]]; then echo "ok   refused: batch name outside [a-z0-9]"; else echo "FAIL batch name"; fail=1; fi

# Check-only mode must fail closed: exit 4, and the command is not run.
marker=$(mktemp -u)
out=$(E1_TEST_DB_CHECK_ONLY=1 E1_TEST_ADMIN_URL="postgres://u@127.0.0.1:5433/postgres" "$SCRIPT" selftest -- touch "$marker" 2>&1)
rc=$?
if [ "$rc" -eq 4 ] && [ ! -e "$marker" ]; then echo "ok   check-only exits 4 and runs nothing"; else echo "FAIL check-only ($rc)"; fail=1; fi
rm -f "$marker"

exit $fail
