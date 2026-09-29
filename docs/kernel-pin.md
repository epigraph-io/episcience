# Kernel pin

EpiScience depends on the EpiGraph kernel's crates at ONE git rev (the
`[workspace.dependencies]` block in `Cargo.toml`; `Cargo.lock` must name a
single `epigraph-io/epigraph` rev). The same rev decides the kernel SCHEMA the
tests run on: `scripts/e1-test-db.sh` and CI derive it from `Cargo.lock` and
build the test template with that rev's own `epigraph-migrate`.

## Current pin

| Field | Value |
|---|---|
| Kernel rev | `8b2e5c22dbbd15675d288205750b259e0673d849` (kernel `main`) |
| Max kernel migration at that rev | `110` |
| Crates pinned | `epigraph-core`, `epigraph-auth`, `epigraph-crypto`, `epigraph-db`, `epigraph-engine`, `epigraph-cli`, `epigraph-jobs`, `epigraph-events`, `epigraph-embeddings` |
| Previous pin | `4a93f0388c4f1ae944b06ec830860ea807a47489` (pre-tenancy) |

## The rule for moving it

1. **The pin target's maximum kernel migration must be <= the kernel schema
   head of the database EpiScience is deployed against, at deploy time.**
   EpiScience's migrations and runtime assume the kernel objects that the pinned
   rev's migrations create (the tenancy columns, roles and helper functions
   among them); a pin ahead of the deployed kernel would reference objects the
   database does not have. The deploy runbook checks this before installing
   binaries and refuses otherwise.
2. Record the new rev and its maximum migration here and in the PR body.
3. Move every crate in the block together; never mix revs.
4. The CI kernel checkout and the test template follow `Cargo.lock`
   automatically; there is no second sha to edit.
5. Re-run the full gate: the kernel's read paths take a `Viewer`, and a pin
   bump can change which rows a principal sees.
6. The pinned kernel must satisfy the tenancy contract EpiScience asserts
   (`docs/tenancy-contract.md`). The nightly kernel-HEAD canary shows in
   advance whether the kernel's `main` still does.

## What the bump to this pin changed for EpiScience

- `epigraph_engine::recall::recall` and `belief_query::get_belief` take the
  reader's `Viewer`. The synthesis pipeline passes the synthesis OWNER's
  viewer (`Viewer::resolve(pool, owner)`); an owner that cannot be resolved
  fails the job before any stage runs.
- Kernel reads that EpiScience issues itself (full-text search, notebook
  export, countersignature claim reads, stage-4 claim text, novelty backends)
  carry the kernel's `/* {VISIBILITY:<alias>} */` splice, so a caller reads
  exactly the claims the kernel would show it.
- Access tokens are validated by the kernel's own `epigraph_auth::JwtConfig`,
  and the boot-time secret check is `epigraph_auth::assert_production_secret`.
  The kernel's `EpiGraphClaims` REQUIRES `sub` (a uuid), `iss`, `aud`, `exp`,
  `iat`, `nbf`, `jti`, `scopes` and `client_type`; only `agent_id` and
  `owner_id` are optional. The previous local claims type did not require
  `iat` or `nbf`. A token minted outside the kernel's `issue_access_token`
  (for example a long-lived service token used for tool discovery) must carry
  every required claim or it is refused with 401. `nbf` must be present but is
  not checked against the clock (the kernel does not enable `validate_nbf`).
  `mcp_http_auth_test::discovery_token_missing_a_required_claim_is_refused`
  pins this.
- The kernel schema is never vendored: `migrations/upstream/` and its sync
  script are gone.
