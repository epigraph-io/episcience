#!/usr/bin/env bash
# scripts/ci-run-tests.sh -- the test step of CI (and of the local gate), run
# UNDER scripts/e1-test-db.sh, which exports DATABASE_URL (the run's shared
# clone of the test template) and the harness variables.
#
#   scripts/e1-test-db.sh ci -- scripts/ci-run-tests.sh [cargo test args...]
#
# Runs the workspace tests and refuses a run in which no test executed. There
# is no kernel sidecar since E1f: stage 6 writes its kernel PROV edges and
# events in process, on the synthesis owner's transaction.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${DATABASE_URL:?run under scripts/e1-test-db.sh}"
cd "$REPO"
OUT=$(mktemp)
trap 'rm -f "$OUT"' EXIT

set +e
cargo test --workspace "$@" 2>&1 | tee "$OUT"
rc=${PIPESTATUS[0]}
set -e
# A run in which no test executed is a failure, never a silent pass (for
# example a filter that matches nothing, or a harness that skipped the step).
# `|| true`: no summary line at all (a compile error, an aborted binary) must
# reach the rc and log-tail handling below, not kill the script under pipefail.
ran=$({ grep -E '^test result: ' "$OUT" || true; } | awk '{n += $4 + $6} END {print n + 0}')
echo "[ci-run-tests] tests executed: $ran"
if [ "$rc" = 0 ] && [ "$ran" = 0 ]; then
  echo "[ci-run-tests] REFUSED: zero tests ran" >&2
  rc=1
fi
exit $rc
