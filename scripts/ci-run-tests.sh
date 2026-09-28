#!/usr/bin/env bash
# scripts/ci-run-tests.sh -- the test step of CI (and of the local gate), run
# UNDER scripts/e1-test-db.sh, which exports DATABASE_URL (the run's shared
# clone of the test template) and the harness variables.
#
#   scripts/e1-test-db.sh ci -- scripts/ci-run-tests.sh <kernel-server-binary> [cargo test args...]
#
# Starts the kernel sidecar (the pinned rev's `server`) on :8090 against the
# shared clone, waits for /health, then runs the workspace tests. Stage 6
# still POSTs kernel edges over HTTP, and three phase-0 tests talk to the
# sidecar directly; the sidecar goes away when the worker writes edges
# in-process.
#
# Sidecar environment: the committed dev JWT secret (EPIGRAPH_ALLOW_INSECURE_SECRET=1:
# the kernel refuses it otherwise), a providers file that declares no provider,
# EPIGRAPH_ENV=ci, and background jobs disabled so the sidecar never writes
# kernel tables on its own while the suites run.
set -euo pipefail

SERVER=${1:?usage: $0 <kernel-server-binary> [cargo test args...]}
shift
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${DATABASE_URL:?run under scripts/e1-test-db.sh}"

LOG=$(mktemp)
OUT=$(mktemp)
EPIGRAPH_PORT=8090 \
EPIGRAPH_JWT_SECRET='epigraph-dev-secret-change-in-production!!' \
EPIGRAPH_ALLOW_INSECURE_SECRET=1 \
EPIGRAPH_PROVIDERS_CONFIG="$REPO/ci/providers.toml" \
EPIGRAPH_ENV=ci \
EPIGRAPH_DISABLE_JOBS=1 \
  "$SERVER" >"$LOG" 2>&1 &
SIDECAR=$!
stop() {
  kill "$SIDECAR" 2>/dev/null || true
  wait "$SIDECAR" 2>/dev/null || true
  rm -f "$LOG" "$OUT"
}
trap stop EXIT

up=0
for _ in $(seq 1 90); do
  if curl -sf http://127.0.0.1:8090/health >/dev/null; then up=1; break; fi
  if ! kill -0 "$SIDECAR" 2>/dev/null; then break; fi
  sleep 1
done
if [ "$up" != 1 ]; then
  echo "[ci-run-tests] kernel sidecar did not come up" >&2
  tail -60 "$LOG" | sed -E 's#postgres(ql)?://[^[:space:]]*#postgres://<redacted>#g' >&2
  exit 1
fi
echo "[ci-run-tests] kernel sidecar up on :8090"

set +e
EPIGRAPH_API_URL=http://127.0.0.1:8090 cargo test --workspace "$@" 2>&1 | tee "$OUT"
rc=${PIPESTATUS[0]}
set -e
# A run in which no test executed is a failure, never a silent pass (for
# example a filter that matches nothing, or a harness that skipped the step).
ran=$(grep -E '^test result: ' "$OUT" | awk '{n += $4 + $6} END {print n + 0}')
echo "[ci-run-tests] tests executed: $ran"
if [ "$rc" = 0 ] && [ "$ran" = 0 ]; then
  echo "[ci-run-tests] REFUSED: zero tests ran" >&2
  rc=1
fi
if [ "$rc" != 0 ]; then
  echo "[ci-run-tests] sidecar log tail:" >&2
  tail -60 "$LOG" | sed -E 's#postgres(ql)?://[^[:space:]]*#postgres://<redacted>#g' >&2
fi
exit $rc
