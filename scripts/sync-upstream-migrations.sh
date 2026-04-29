#!/usr/bin/env bash
# Sync vendored upstream migrations to match the SHA pinned in Cargo.toml.
#
# Reads the `epigraph-core = { ..., rev = "..." }` SHA from Cargo.toml,
# fetches the epigraph-io/epigraph repo at that ref, and overwrites
# migrations/upstream/*.sql with the upstream's migrations directory.
#
# Run after bumping the SHA in Cargo.toml.

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_DIR"

SHA=$(grep -E '^epigraph-core\s*=' Cargo.toml | head -1 | sed -E 's/.*rev = "([0-9a-f]+)".*/\1/')
if [[ -z "$SHA" || "$SHA" == *"epigraph-core"* ]]; then
  echo "ERROR: could not parse epigraph SHA from Cargo.toml" >&2
  exit 1
fi
echo "Syncing upstream migrations from epigraph-io/epigraph @ $SHA"

WORK=$(mktemp -d)
trap "rm -rf $WORK" EXIT

git clone --filter=blob:none --depth 1 --no-checkout https://github.com/epigraph-io/epigraph "$WORK/repo"
cd "$WORK/repo"
git fetch --depth 1 origin "$SHA"
git checkout "$SHA" -- migrations/

cd "$REPO_DIR"
rm -f migrations/upstream/*.sql
cp "$WORK/repo/migrations"/*.sql migrations/upstream/

echo "Synced. New file count: $(ls migrations/upstream/*.sql | wc -l)"
