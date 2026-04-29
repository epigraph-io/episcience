# Vendored upstream migrations

Mirror of `epigraph-io/epigraph/migrations/*.sql` at SHA `519cba8ad206d87c6fdd398a73970a0654ee1cbd`
(head of `feat/phase0-integrated`, PR #10 on epigraph-io/epigraph).

These migrations create the upstream schema (`claims`, `evidence`, `agents`, `frames`,
`mass_functions`, `events`, etc.) that the episcience pipeline depends on for:
- `recall` (queries `claims` and `evidence`)
- `get_belief` (queries `mass_functions` joined to `frames` and `claims`)
- PROV-O edge writes (references `agents` for `ATTRIBUTED_TO`)
- Cross-source links surfaced through synthesis cluster traversal

## Apply order

```
migrations/upstream/001..016         # epigraph schema
migrations/5001..5010                # episcience ELN (samples, protocols, blobs, countersignatures)
migrations/synthesis/5011..5019      # synthesis pipeline + failure_reason
```

## Updating

When the workspace `epigraph-*` deps in `Cargo.toml` are repinned to a new SHA, re-run:
```bash
./scripts/sync-upstream-migrations.sh
```
This fetches the upstream repo at the SHA in `Cargo.toml` and overwrites this directory.
