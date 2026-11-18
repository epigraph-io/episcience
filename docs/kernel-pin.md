# Kernel pin

EpiScience depends on the EpiGraph kernel's crates at ONE git rev (the
`[workspace.dependencies]` block in `Cargo.toml`; `Cargo.lock` must name a
single `epigraph-io/epigraph` rev). The same rev decides the kernel SCHEMA the
tests run on: `scripts/e1-test-db.sh` and CI derive it from `Cargo.lock` and
build the test template with that rev's own `epigraph-migrate`.

## Current pin

| Field | Value |
|---|---|
| Kernel rev | `1ace184f0d4ad1807257397ad08f1cd6aba4b3a2` (kernel `main`, the merge of epigraph#928) |
| Max kernel migration at that rev | `229` |
| Crates pinned | `epigraph-core`, `epigraph-auth`, `epigraph-crypto`, `epigraph-db`, `epigraph-engine`, `epigraph-cli`, `epigraph-jobs`, `epigraph-events`, `epigraph-embeddings` |
| Previous pin | `55b524f3774a1ea5de2fbdae93c3304c7f9a5028` (max migration `208`) |
| Pin before that | `7dd79226ad939b402971f63ef4f64bf86c5530ea` (max migration `185`) |
| Earlier | `b9e056a047a4a17225e16a4d0cc9861b19ba2b96` (max migration `168`), `8b2e5c22dbbd15675d288205750b259e0673d849` (max migration `110`), `4a93f0388c4f1ae944b06ec830860ea807a47489` (pre-tenancy) |

Why this rev: it is the kernel build deployed to production, whose maximum
migration (229) is the kernel schema head of the database EpiScience is
deployed against (rule 1 below). The previous pin, `55b524f3`, was 1168
commits behind it (47 first-parent merges).

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
  Retries are unchanged: the pipeline retries only `MalformedResponse`, and
  the job layer maps every LLM error to `ProcessingFailed` as before. A
  failed synthesis's stored error text for a stop reads "LLM refused the
  request: claude -p stopped by safety classifier: ..." where it read "LLM
  API request failed: claude -p stopped by safety classifier: ...".
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
