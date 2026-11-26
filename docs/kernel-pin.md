# Kernel pin

EpiScience depends on the EpiGraph kernel's crates at ONE git rev (the
`[workspace.dependencies]` block in `Cargo.toml`; `Cargo.lock` must name a
single `epigraph-io/epigraph` rev). The same rev decides the kernel SCHEMA the
tests run on: `scripts/e1-test-db.sh` and CI derive it from `Cargo.lock` and
build the test template with that rev's own `epigraph-migrate`.

## Current pin

| Field | Value |
|---|---|
| Kernel rev | `efb62471e193a86ee7e9c2f568be346e0f96e446` (kernel `main`, the merge of epigraph#1113) |
| Max kernel migration at that rev | `348` (308-348 new; 286-307 do not exist) |
| Crates pinned | `epigraph-core`, `epigraph-auth`, `epigraph-crypto`, `epigraph-db`, `epigraph-engine`, `epigraph-cli`, `epigraph-jobs`, `epigraph-events`, `epigraph-embeddings` |
| Previous pin | `c8379f2dbe7f57558dff7e60a9b586dac528f699` (max migration `285`) |
| Pin before that | `ffeb7bb3a2eea58ae31069d9022e6012ec5a4ee7` (max migration `271`) |
| Earlier | `1ace184f0d4ad1807257397ad08f1cd6aba4b3a2` (max migration `229`), `55b524f3774a1ea5de2fbdae93c3304c7f9a5028` (max migration `208`), `7dd79226ad939b402971f63ef4f64bf86c5530ea` (max migration `185`), `b9e056a047a4a17225e16a4d0cc9861b19ba2b96` (max migration `168`), `8b2e5c22dbbd15675d288205750b259e0673d849` (max migration `110`), `4a93f0388c4f1ae944b06ec830860ea807a47489` (pre-tenancy) |

Why this rev: it is the kernel build that takes the production database to
migration 348. Rule 1 below therefore orders the deploy: this EpiScience
build is installed only AFTER the kernel's migrator has taken the database
to 348. Unlike earlier bumps, the previous build cannot keep running across
that migration: on a 346+ database the `c8379f2d` kernel library resolves
viewers without their operator-reached teams (see below), so the previous
EpiScience build stays stopped from the kernel deploy until this one is
installed.

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

## What the bump to `efb62471` changed for EpiScience

- **Why it was needed.** Kernel migration 346 (epigraph #1105, written as
  309 there) binds `epigraph_readable_teams(agent)` and
  `epigraph_writable_teams(agent)` to the SESSION principal: an unstamped
  call returns no rows. At `c8379f2d`,
  `GroupMembershipRepository::list_operator_readable_teams` (one of the
  reads behind `Viewer::resolve`) calls `epigraph_readable_teams($1)` on the
  plain pool, and EpiScience resolves on its UNSTAMPED resolve pool
  (`EpiscienceDb::resolve_principal`, `jobs::session`, `jobs::worker`). On a
  346 database the `c8379f2d` build would therefore resolve every operated
  principal without the teams its live human reads, with no error. At
  `efb62471` (it contains #1105's `308fbba33`) that read stamps the agent it
  resolves, transaction-locally, in the same statement, as
  `list_live_for_agent` has since 236. 346's own text says it: "any other
  service linking epigraph-db at an older revision (bump its pin first)".
  The exposure is narrow today, because every entry path refuses an operated
  principal before it reads (`RequestRefusal::Operated`,
  `Refusal::Operated`; see the `ffeb7bb3` notes below). The exception is
  the open follow-up recorded there: a job's later stages re-resolve without
  re-checking the operator link. The pin still moves with the deploy, and
  the old build stays stopped until this one is installed.
- **What changed in EpiScience: two test assertions, no production source.**
  The workspace compiles, and passes fmt and clippy `-D warnings`.
  `Cargo.lock` moves only the rev of the 17 kernel git packages: no package
  added, removed or re-versioned, and no dependency edge changed. The
  kernel's `Cargo.toml`, `crates/*/Cargo.toml` and `Cargo.lock` are identical
  at `c8379f2d` and `efb62471`.
- **The no-DS belief fallback (epigraph #1108).**
  `belief_query::get_belief` without a frame now reads a claim with no DS
  columns as the least-committed BBA whose BetP is its `truth_value`. For
  t > 1/2 that is `[2t - 1, 1]`, for t < 1/2 it is `[0, 2t]`, and `pignistic_prob`
  stays t. Before, it read `[t, 1]`. The seeded claim `aaaaaaaa-...`
  (t = 0.8) now reads Bel 0.6, Pl 1, BetP 0.8, and two tests assert that:
  `phase01_e2e_test::test_phase0_library_get_belief_callable` and
  `synthesis_pipeline_stage2_test::stage2_traverse_persists_snapshot_and_membership`.
  Stage 2 records the new Bel/Pl in `subgraph_snapshot.belief_intervals` for
  NEW syntheses. Existing syntheses are not marked stale by the change:
  `staleness::recheck_beliefs` compares only `pignistic_prob`, and that value
  did not change.
- **The maintenance pool fails closed (epigraph #1103).**
  `ScopedPool::unscoped_for_maintenance` and `maintenance_session` now return
  `DbError::MaintenancePoolNotAttached` when no maintenance pool is attached.
  Before, they fell back to the application pool, and `maintenance_inner` is
  gone. EpiScience calls none of the three and attaches no maintenance pool
  (`with_maintenance_pool`): its two production `ScopedPool`s
  (`EpiscienceDb::connect`, behind the REST and MCP servers, and
  `episcience-worker`'s, which `StageSession` draws from) serve application
  and stamped reads only. `episcience-maint` runs on its own plain
  connection, not through a `ScopedPool`. The kernel's own
  callers are in `epigraph-api`, `epigraph-mcp`, the `epigraph-cli`
  `MaintenancePool` (which attaches its pool), and `epigraph-jobs`'
  reputation and privatization jobs. EpiScience reaches none of them, so no
  EpiScience path changes.
- **Kernel migrations 308 to 344** were deployed with kernel `313dd0bf`
  (epigraph #1088 and #1111). The `c8379f2d` build (sci7) already passed the
  full kernel-compatibility suite on a 344 schema, so none of them needs an
  EpiScience change. In outline:
  - 308 keys `ds_combined_beliefs` NULLS NOT DISTINCT;
  - 309-311, 314, 316, 317, 320-322, 325, 332, 334, 335, 337, 339, 343 and
    344 are the human-identity disclosure definers, profile visibility, the
    `agents.profile_public_consented_at` stamp and its INSERT policy, and
    the team-invite answer. EpiScience reads `agents` only through C13's
    columns and inserts no `agents` row;
  - 312, 313, 323, 324, 326-331, 333, 336, 338, 340-342 add or drop indexes;
  - 315, 318 and 319 restrict the role-projection edges (EpiScience writes
    none).
- **Kernel migrations 345 to 348 name no EpiScience object** and no object a
  tenancy-contract item (C1-C15, L1, S1, S2) names. EpiScience pins no body
  or md5 of any function they re-state:
  - 345 (epigraph #1104) takes INSERT/UPDATE/DELETE/TRUNCATE on
    `bp_messages` and `UPDATE (frame_id)` on `factors` from `epigraph_app`,
    and adds `epigraph_promote_hypothesis_factors`. EpiScience writes neither
    table: 217 had already taken the rest of `factors`, and EpiScience's 5040
    dropped the trigger that wrote it;
  - 346 (epigraph #1105): the two team lists, as above, and
    `epigraph_is_group_admin(group)` now decides on the principal's live
    membership. EpiScience calls none of the three directly (C12's
    `epigraph_live_memberships` keeps 236's signature and grant);
  - 347 (epigraph #1106) puts `harvester_enriched_concepts` under tenancy
    (a kernel table that EpiScience does not read);
  - 348 (epigraph #1109) re-states `oauth_clients.agents_must_have_owner` as
    "an ACTIVE agent client has an owner", and adds a 4-argument
    `epigraph_oauth_client_approve` overload (maintenance only). EpiScience
    validates tokens with the kernel's `JwtConfig` and writes no
    `oauth_clients` row.
- **Kernel library changes EpiScience reaches:** none of the kernel functions
  EpiScience calls (listed under `c8379f2d` below) changed its signature.
  `epigraph-jobs` gained retry-after-aware backoff helpers (epigraph #1110);
  EpiScience uses only `JobError`, `JobResult` and `JobResultMetadata`, and
  runs its own queue.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 308 to 348.

## What the bump to `c8379f2d` changed for EpiScience

- **Why.** The kernel deploy of 2026-11-25 takes production from migration
  271 to 285 (kernel #1081 and #1096). Rule 1 lets EpiScience keep a pin
  below the deployed head, but the kernel's library code at `ffeb7bb3` then
  differs from what the database runs, and every later pin move would have to
  cross more of it at once. Moving with the deploy keeps the pin equal to the
  deployed kernel.
- **What changed in EpiScience: no source.** The workspace compiles, and
  passes fmt and clippy `-D warnings`, unchanged. `Cargo.lock` moves only the
  rev of the 17 kernel git packages: no package added, removed or re-versioned,
  no dependency edge changed.
- **Kernel migrations 272 to 285 name no EpiScience object** and no object a
  tenancy-contract item (C1-C15, L1, S1, S2) names:
  - 272 adds `bug_report_intake_marks` and re-states the bug-report definers;
  - 273 adds `epigraph_group_live_roster` (the kernel's group gates and the
    pool forms `is_member` / `get_member_role` / `get_members`, which now answer
    only on a maintenance or superuser session; EpiScience calls none of them);
  - 274 confines the retracted-BBA guard's mark arm to a ledger grant
    (EpiScience writes no BBA);
  - 275 sets `ROWS 1` on `epigraph_operator_actor` (planner estimate only; it
    lowers the cost of every `epigraph_live_memberships` call, which
    `Viewer::resolve` makes);
  - 276 adds the one-file prompt-history definer;
  - 284 re-states the edge tenancy re-meets (`epigraph_edges_tenancy`,
    `epigraph_propagate_tenancy`, publicize, share) so a DECLARED edge is only
    narrowed; EpiScience declares no edge tenancy (260's columns), so its edges
    re-meet as before;
  - 285 records a `none` passkey's anchor (passkeys are kernel-only).
- **Kernel library changes EpiScience reaches:** none of the kernel functions
  EpiScience calls (`recall::recall`, `ClaimRepository::create`/`create_conn`/
  `content_as`/`content_and_pair_as`/`theme_for_wiki_as`/
  `default_decl_for_author`/`agent_public_key`,
  `EventRepository::publish_or_log_conn`, `Viewer::resolve`) changed its
  signature. `EventRepository::list` now returns a `tool.invoked` row only to
  its actor (kernel #1093); EpiScience publishes events and never lists them.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 272 to 285.

## What the bump to `ffeb7bb3` changed for EpiScience

- **Why it was needed.** Kernel migration 236 binds
  `public.epigraph_live_memberships(agent)` to the SESSION principal: an
  application session no longer lists the memberships of an agent it has not
  stamped as `epigraph.principal_id`. The kernel's `Viewer::resolve` at
  `1ace184f` read the definer on an unstamped connection, so on a 236+
  schema it returned no row for anyone: every principal resolved
  public-only (no personal group, no team), a member lost its private
  groups, and `EpiscienceDb::write_as` refused every write (empty writable
  set). The REST server, the MCP server and the worker all resolve through
  it (`EpiscienceDb::resolve_principal`, `jobs::session`, `jobs::worker`).
- **What changed in EpiScience: no source.** From the kernel's 236 change on,
  `GroupMembershipRepository::list_live_for_agent` (the one statement behind
  `Viewer::resolve`) stamps the principal it resolves, transaction-locally,
  in the same statement that calls the definer. EpiScience's call sites keep
  calling `Viewer::resolve` on the unstamped resolve pool, exactly as the
  kernel's bearer middleware does, and are correct unchanged. EpiScience
  issues no membership read of its own on the application path.
  `request_stamping_test::a_member_resolves_and_reads_its_private_group_and_an_outsider_does_not`
  pins it on the application login: it fails on the `1ace184f` pin against a
  271 schema (the member resolves no group) and passes on this one.
- **`Viewer::resolve` also reads more for an OPERATED principal.** Since
  kernel 234, `Viewer::resolve` adds
  `GroupMembershipRepository::list_operator_readable_teams` to the read set:
  an operated agent reads every team its live human reads (at `1ace184f` it
  gained only the human's personal group). This is moot for EpiScience
  today, because every entry path refuses an operated principal before it
  reads anything: `EpiscienceDb::resolve_principal` (REST and MCP) returns
  `RequestRefusal::Operated`, and `Worker::authorize` returns
  `Refusal::Operated` when a job starts. Relaxing either refusal now hands an
  operated agent its human's team-private claims, not only the human's
  personal group; decide that on purpose. A job's later stages
  (`StageSession::viewer`, `StageSession::begin`) re-resolve the acting
  principal without re-checking its operator link, so a link created while a
  job runs widens that job's later reads the same way (open follow-up).
- **The tenancy contract probe is unchanged.** C1-C15, L1, S1 and S2 pass on a
  271 kernel with both pins (`kernel_contract`,
  `boot_contract_probe_test`). 236 keeps `epigraph_live_memberships(uuid)`'s
  signature and the application role's EXECUTE (C12); 248 (attestor rules),
  267 (`mass_functions` slot columns) and 271 (`pg_temp` last on every
  pinned `search_path`) touch no object a contract item names.
- The workspace compiled, and passed fmt and clippy, with no source change.
  `Cargo.lock` moved only the kernel rev and new dependency edges of crates
  already in the lock (no package added or removed): `epigraph-auth` now
  depends on `base64`, `ed25519-dalek`, `hex` and `serde_json`,
  `epigraph-cli` on `epigraph-jobs`, `epigraph-mcp` on `reqwest` 0.13 and
  `rustix`, and `epigraph-crypto`, `blake3` and `arrayvec` on `zeroize`.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 230 to 271.

## What the bump to `1ace184f` changed for EpiScience

- `LlmProvider` gained a defaulted `complete_json_schema(prompt, schema)`
  with the signature of EpiScience's `SchemaLlm` shim, and `LlmError` gained
  `Refusal { message }`. With both traits in scope the shim made the call
  ambiguous (E0034), so `SchemaLlm` is gone:
  - the synthesis pipeline is bounded on `LlmProvider`;
  - `ClaudeCliProvider` overrides `complete_json_schema` (its
    `--json-schema` call);
  - every forwarding wrapper (`ArcLlm`, `RecordingLlmClient`, the
    tests' gates) forwards it, because the kernel default calls
    `complete_json` and drops the schema;
  - a safety-classifier stop is now `LlmError::Refusal` (message still
    prefixed `CLASSIFIER_STOP_PREFIX`) instead of `RequestFailed`.
  The bump itself left retries unchanged: the pipeline retried only
  `MalformedResponse`, and the job layer still mapped every LLM error to
  `ProcessingFailed`, so a refused job was re-run (and paid for) until
  `max_attempts`. A stop's stored error text read "LLM error: LLM refused
  the request: claude -p stopped by safety classifier: ..." where it read
  "LLM error: LLM API request failed: claude -p stopped by safety
  classifier: ...".
  **Superseded by a later change:** a refusal is now its own
  `SynthesisError::Refused`, which the job layer maps to
  `JobError::PermanentFailure`, so the worker ends the job `failed` after one
  attempt instead of retrying it. Its text reads "LLM refused the request:
  claude -p stopped by safety classifier: ..." (no "LLM error: " prefix).
  `MalformedResponse` is still retried within the prompt, and transport,
  rate-limit, no-provider and missing-key errors still map to
  `ProcessingFailed` and are retried by the job runner.
- `EmbeddingService` lost `store`, `get` and `similar` (and `SimilarClaim`,
  `EmbeddingError::NotFound`). Stage 2's relevance closure called
  `get`; no production embedder implemented it (`OpenAiProvider` returned
  `ConfigError`, `MockProvider` read an empty in-memory map), so any
  neighbour would have scored 0.0. Stage 2 now reads each neighbour's
  stored `claims.embedding` as the synthesis owner
  (`ClaimThemeRepository::get_claim_embedding_str`, stage 1's theme-seed
  read). A neighbour the owner cannot read, one with no stored embedding,
  and a failed read are pruned.
  This is NOT a runtime change today. `episcience-worker` builds its
  handler with `EmptyEdgeProvider`, which returns no neighbours, so stage 2
  stays seeds-only and the relevance read never runs in production. The
  port takes effect only when a real edge provider ships (B-CKL Phase 4).
  At that point note two things: the worker's engine pool returns only
  public claims, so only public neighbours with a stored embedding of the
  query's dimension survive; and a failed read (pool timeout, dropped
  connection) prunes the neighbour with a `warn!` rather than failing the
  stage, so a transient database fault yields a smaller snapshot that is
  persisted as a success. Decide then whether that should fail the stage.
- `Normalizer::format_as_pgvector` is gone; the theme seed formats its query
  through `epigraph_db::pgvector_literal` (the same text).
- `epigraph_engine::recall::recall` keeps its signature and stays on the
  similarity (ANN) route: the theme-traversal leg the kernel added is opt-in
  (`recall_routed`), so stage-1 seeding reads what it read before. The diverse
  theme reads now state `is_current` and run as iterative HNSW scans; the
  stage-1 theme seed's `claims_in_themes_at_dim` is the flat read, unchanged.
- The kernel OpenAI client (`OpenAiProvider`) now truncates over-limit input
  and retries 429/5xx itself; EpiScience's synthesis query embedding goes
  through it when `EPISCIENCE_EMBED_MODE=openai`. The `openai` feature was
  already on through the kernel's workspace dependency and is now the
  crate default.
- `Cargo.lock` moved only the kernel rev, dropped `csv`/`csv-core` (no
  longer pulled in by the kernel) and added the kernel's
  `epigraph-atlas-contract`.
- Kernel migrations 209 to 229 name no EpiScience object. The table lists in
  218 and 219 (`experiment_triples`, `experiment_entity_mentions`) are kernel
  tables. 210 enables row security only on the three new `atlas_map_*`
  tables. 216 makes the tenancy seed escape require an EXPLICIT
  `epigraph_seed` grant (a superuser no longer counts), so an undeclared claim
  insert on a superuser session now fails with 23502; EpiScience declares
  tenancy on every claim it writes, and `scripts/ci-seed.sql` still loads.
  212 refuses vector writes to sealed claim and evidence rows, and 227
  serializes a seal against a publicize of the same claim; EpiScience
  writes no kernel vectors (its one vector write is its own
  `synthesis_embeddings`) and neither seals nor publicizes claims. 220 adds `jobs.redelivery_count` to the kernel queue,
  which EpiScience does not use (it runs its own queue definers). 217
  revokes the application role's INSERT/UPDATE/DELETE on `factors`: the
  legacy `create_shared_evidence_factor` invoker trigger (dropped by
  EpiScience migration 5040) would now refuse every application-role
  evidence edge insert instead of deriving a factor; only the 5040 undo
  fixture in `shared_evidence_detach_test` still creates it.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 209 to 229.

## What the bump to `55b524f3` changed for EpiScience

- No kernel API EpiScience calls changed shape: the workspace compiled, and
  passed clippy and its full suite, with no source change. `Cargo.lock` moved
  only the kernel rev, the kernel's own `rmcp` (2.2 to 3.9, through
  `epigraph-cli` -> `epigraph-mcp`; no type crosses to EpiScience's `rmcp` 0.15) and what
  `rmcp` 3.9 brings (`sha2` 0.11, `digest` 0.11, `syn` 3, `darling` 0.24).
- `epigraph_engine::recall::recall`'s text-search fallback (taken when the
  embedder fails) now serves CURRENT claims only, filtered in SQL before
  `LIMIT`; it used `ClaimRepository::list`, which also returned superseded
  revisions. The semantic leg was already current-only. A stage-1 seed taken
  on the fallback can therefore differ from the one at `7dd79226`.
- Kernel migrations 186 to 208 add, among others: `events` append-only for
  the application role (189; EpiScience only INSERTs events), team-default
  claim visibility definers (206; EpiScience passes an explicit `TenancyDecl`
  on every claim it writes), and the ingest-marker edge guard and authorship
  countersign route (198, 208; kernel paper authorship, unrelated to
  EpiScience's countersignatures). None of them names an EpiScience object.
  The one catalog-wide statement, 190's sweep, appends `pg_temp` to every
  SECURITY DEFINER `search_path` that does not end in it; every EpiScience
  definer pins `public, pg_temp` (`catalog::PINNED_SEARCH_PATH`), so on a
  database where EpiScience's migrations ran before 190 the sweep skips them.
  The tenancy contract (`kernel_contract` tests) holds at 208.
- The test template is built by this rev's `epigraph-migrate`, so it now has
  kernel migrations 186 to 208.

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
