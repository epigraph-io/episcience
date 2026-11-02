# Kernel pin

EpiScience depends on the EpiGraph kernel's crates at ONE git rev (the
`[workspace.dependencies]` block in `Cargo.toml`; `Cargo.lock` must name a
single `epigraph-io/epigraph` rev). The same rev decides the kernel SCHEMA the
tests run on: `scripts/e1-test-db.sh` and CI derive it from `Cargo.lock` and
build the test template with that rev's own `epigraph-migrate`.

## Current pin

| Field | Value |
|---|---|
| Kernel rev | `7dd79226ad939b402971f63ef4f64bf86c5530ea` (kernel `main`, the merge of epigraph#719) |
| Max kernel migration at that rev | `185` |
| Crates pinned | `epigraph-core`, `epigraph-auth`, `epigraph-crypto`, `epigraph-db`, `epigraph-engine`, `epigraph-cli`, `epigraph-jobs`, `epigraph-events`, `epigraph-embeddings` |
| Previous pin | `b9e056a047a4a17225e16a4d0cc9861b19ba2b96` (max migration `168`) |
| Pin before that | `8b2e5c22dbbd15675d288205750b259e0673d849` (max migration `110`) |
| Earlier | `4a93f0388c4f1ae944b06ec830860ea807a47489` (pre-tenancy) |

Why this rev: it is a real kernel `main` commit, its maximum migration (185)
is not ahead of the kernel schema head of the database EpiScience is deployed
against (rule 1 below), and it is the newest such commit: the next kernel
`main` commit adds migration 186. It is the first pin whose library recall
(`epigraph_engine::recall::recall`) writes its audit row as the caller's
principal (backlog 41841f7b; see below).

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

## What the bump to `7dd79226` changed for EpiScience

- `epigraph_engine::recall::recall` takes a second argument, the caller's
  `&ScopedPool`, and writes its `recall_events` audit row on a transaction of
  it stamped as the viewer's principal: `agent_id` = that principal,
  `owner_group_id` = its personal group (backlog 41841f7b). At `b9e056a0` the
  row was written agent-less on the unstamped engine pool, which
  `recall_events_tenancy` refuses for the application role, so no
  EpiScience synthesis was audited. `SynthesisPipeline::stage1_seed` now takes
  the `ScopedPool` its stage transactions come from
  (`StageSession::scoped`) and passes it with the ACTING principal's viewer,
  so the row names the identity the stage transactions run as.
  `worker_test::a_worker_synthesis_writes_one_recall_audit_row_as_the_acting_principal`
  pins exactly one row per worker synthesis on the unprivileged worker login.
- The kernel's `zstd` floor moved to 0.14.3 (`zstd-safe` 8.2), and
  `epigraph-engine` no longer carries the theme k-means (`linfa`, `ndarray`);
  EpiScience uses neither directly.
- No other kernel API EpiScience calls changed shape: the workspace compiled
  unchanged apart from the recall call sites.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 169 to 185.

## What the bump to `b9e056a0` changed for EpiScience

- `JwtConfig::issue_access_token` takes a seventh argument, an
  `AccessTokenBinding` (the refresh family, elevation and delegator a token is
  bound to). EpiScience mints no tokens; its one call site is a unit test,
  which passes `AccessTokenBinding::default()`.
- `EpiGraphClaims` gained three OPTIONAL claims, `fam`, `elv` and `dlg`
  (`#[serde(default)]`, so a token without them still decodes). The required
  set is unchanged. EpiScience reads none of them: a delegated run token
  (`dlg`) is treated as a token for its `agent_id`, the case the kernel
  documents as failing closed when that agent is operated, and EpiScience
  refuses every operated principal (`EpiscienceDb::resolve_principal`).
- The kernel crates' graph now carries `rmcp` 2.2 and `jsonwebtoken` 10 (through
  `epigraph-cli` and `epigraph-auth`) next to EpiScience's own `rmcp` 0.15 and
  `jsonwebtoken` 9. No type crosses between the copies: tokens are validated by
  the kernel's `JwtConfig` alone.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 111 to 168.

## What the bump to `8b2e5c22` changed for EpiScience

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
  `owner_id` are optional (since `b9e056a0` also `fam`, `elv` and
  `dlg`). The previous local claims type did not require `iat` or `nbf`. A token minted outside the kernel's `issue_access_token`
  (for example a long-lived service token used for tool discovery) must carry
  every required claim or it is refused with 401. `nbf` must be present but is
  not checked against the clock (the kernel does not enable `validate_nbf`).
  `mcp_http_auth_test::discovery_token_missing_a_required_claim_is_refused`
  pins this.
- The kernel schema is never vendored: `migrations/upstream/` and its sync
  script are gone.
