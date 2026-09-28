#!/usr/bin/env bash
# scripts/e1-test-db-selftest.sh -- the admin-URL refusals of scripts/e1-test-db.sh,
# checked WITHOUT any database: every case below is refused before the script
# runs psql, cargo or anything else, and one accepted URL (with
# E1_TEST_DB_CHECK_ONLY=1, which stops right after the checks) proves the
# refusals come from the URL and not from an unrelated early exit.
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
  if [ "$rc" -eq 0 ] && [[ "$out" == *"admin URL accepted"* ]]; then
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
expect_accepted "single host on 5433"            "postgres://u:p@127.0.0.1:5433/postgres"
expect_accepted "postgresql:// scheme on 5433"   "postgresql://u@localhost:5433/postgres"

# The batch name is folded into every database name; a bad one is refused too.
out=$(E1_TEST_DB_CHECK_ONLY=1 E1_TEST_ADMIN_URL="postgres://u@127.0.0.1:5433/postgres" "$SCRIPT" 'Bad-Name' -- true 2>&1)
if [ $? -eq 2 ] && [[ "$out" == *REFUSED* ]]; then echo "ok   refused: batch name outside [a-z0-9]"; else echo "FAIL batch name"; fail=1; fi

exit $fail
