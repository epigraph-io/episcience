# Quickstart — episcience extension

This guide assumes you've completed the [EpiGraph quickstart](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/01-quickstart.md) and have a running kernel on `postgres://epigraph:epigraph@localhost/epigraph` with the API listening on `127.0.0.1:8080`.

Time budget: ~5 minutes if the kernel is already running.

## Prerequisites

- A completed EpiGraph quickstart (kernel migrations applied, API server running on `127.0.0.1:8080`)
- `psql` on your `$PATH` (you already have it from EpiGraph Step 1), to create the application logins in Step 3

## Step 1 — Clone episcience

```bash
git clone https://github.com/epigraph-io/episcience.git
cd episcience
```

The workspace pins specific epigraph crates by git rev in the committed `Cargo.toml` (see lines 28-35). If you're hacking on the kernel locally too, override the pin in `~/.cargo/config.toml` — **not** in the committed workspace `Cargo.toml`:

```toml
[patch."https://github.com/epigraph-io/epigraph"]
epigraph-core       = { path = "/home/youruser/epigraph/crates/epigraph-core" }
epigraph-crypto     = { path = "/home/youruser/epigraph/crates/epigraph-crypto" }
epigraph-db         = { path = "/home/youruser/epigraph/crates/epigraph-db" }
epigraph-engine     = { path = "/home/youruser/epigraph/crates/epigraph-engine" }
epigraph-cli        = { path = "/home/youruser/epigraph/crates/epigraph-cli" }
epigraph-jobs       = { path = "/home/youruser/epigraph/crates/epigraph-jobs" }
epigraph-events     = { path = "/home/youruser/epigraph/crates/epigraph-events" }
epigraph-embeddings = { path = "/home/youruser/epigraph/crates/epigraph-embeddings" }
```

Don't commit personal patches — keep the committed values CI-canonical.

## Step 2 — Apply episcience migrations

Episcience layers on top of the kernel schema that the EpiGraph quickstart already applied (by the kernel's `epigraph-migrate`). Apply EpiScience's own schema with `episcience-migrate`:

```bash
env -u DATABASE_URL \
  EPISCIENCE_MIGRATION_DATABASE_URL=postgres://epigraph:epigraph@localhost/epigraph \
  cargo run --release -p episcience-api --bin episcience-migrate -- run
```

- EpiScience keeps its OWN ledger, `episcience_meta._sqlx_migrations`. The kernel's `public._sqlx_migrations` is never written: the kernel's migrator refuses a database holding versions it does not embed.
- The migrator reads only `EPISCIENCE_MIGRATION_DATABASE_URL` and refuses to start while `DATABASE_URL` is set.
- A database whose EpiScience tables were built by the old hand-applied files is adopted instead of migrated: `episcience-migrate adopt-baseline` compares the live tables with `migrations/5032_legacy_baseline.fingerprint` and records the baseline only on an exact match.
- `episcience-migrate status` lists embedded, recorded and pending versions.

See `migrations/README.md` for the layout and version ranges. If you see a "relation does not exist" error mentioning a kernel table (e.g. `claims`, `agents`), the kernel migrations weren't applied first — go back to the [EpiGraph Step 3](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/01-quickstart.md#step-3--migrations).

## Step 3 — Create the application logins

EpiScience never runs on a superuser: the server and the MCP server run on an
application login (`episcience_app`), the synthesis worker on its own
(`episcience_worker`), and every binary refuses a superuser, BYPASSRLS or
kernel-maintenance session at boot. Migration 5033 (applied in Step 2) created
the NOLOGIN roles these logins join. For a local development database (pick
your own passwords; production logins are created out of band with generated
passwords):

```sql
CREATE ROLE episcience_app    LOGIN PASSWORD '<app password>'
  NOSUPERUSER NOBYPASSRLS NOCREATEROLE NOCREATEDB INHERIT;
CREATE ROLE episcience_worker LOGIN PASSWORD '<worker password>'
  NOSUPERUSER NOBYPASSRLS NOCREATEROLE NOCREATEDB INHERIT;
GRANT epigraph_app, episcience_rw TO episcience_app;
GRANT epigraph_app, episcience_rw, episcience_queue TO episcience_worker;
```

## Step 4 — Build and start the API and the worker

```bash
cargo build --release -p episcience-api
```

This produces the binaries under `target/release/`: `episcience-server` (the HTTP API),
`episcience-mcp-server` (the MCP server for Claude Code), `episcience-worker` (the synthesis
queue), `episcience-migrate` (Step 2) and `episcience-maint` (the maintenance timer's tick).

```bash
export EPIGRAPH_JWT_SECRET=<your EpiGraph API's EPIGRAPH_JWT_SECRET>   # required: no fallback

EPISCIENCE_PORT=8091 \
DATABASE_URL=postgres://episcience_app:<app password>@localhost/epigraph \
  cargo run --release -p episcience-api --bin episcience-server
```

In a second shell, the synthesis worker (it reads ONLY its own DSN variable, and refuses to start
while `DATABASE_URL` is set, so do not export that one globally):

```bash
EPISCIENCE_WORKER_DATABASE_URL=postgres://episcience_worker:<worker password>@localhost/epigraph \
  cargo run --release -p episcience-api --bin episcience-worker
```

In another shell:

```bash
curl http://127.0.0.1:8091/health
```

Expected: an HTTP 200 with body `{"status":"healthy","service":"episcience-eln","version":"…"}`.

Notes on the env vars above:

- `EPISCIENCE_PORT` — port for the episcience HTTP server. Defaults to `8081` in `src/bin/server.rs`. We pick `8091` here so it doesn't collide with EpiGraph on `8080`.
- `EPIGRAPH_JWT_SECRET` — the secret your EpiGraph API signs access tokens with. Required: the server exits at boot without it, and refuses one shorter than 32 bytes or equal to the kernel's committed development literal (so a local EpiGraph API must also run with a real secret, e.g. `openssl rand -hex 32`). Requests must carry an EpiGraph access token (`iss=epigraph`, `aud=epigraph-api`, with an `agent_id`); `GET`s need the `claims:read` scope and writes need `claims:write`.
- The server listens on `127.0.0.1` by default (`EPISCIENCE_BIND_ADDR`); `0.0.0.0` / `::` are refused.
- No kernel service credential: the worker writes the kernel PROV-O edges and their events in process, on
  a transaction stamped as the synthesis' owner. The retired service-client variables
  (`EPIGRAPH_CLIENT_ID`, `EPIGRAPH_CLIENT_SECRET`, `EPIGRAPH_SERVICE_TOKEN`, `EPIGRAPH_SERVICE_AGENT_ID`)
  make every binary refuse to start; `EPIGRAPH_API_URL` is no longer read.

The server also accepts `EPISCIENCE_BLOB_DIR` (default `/var/lib/episcience/blobs`), `EPISCIENCE_MAX_UPLOAD_BYTES` (default 100 MB) and `EPISCIENCE_EMBED_MODE=openai` + `OPENAI_API_KEY` (defaults to a mock embedder, for its search route). The worker takes the model settings: `EPISCIENCE_LLM_MODE=anthropic` + `ANTHROPIC_API_KEY` (defaults to a mock LLM) and the same embedder variables. For the verification step below, the mock LLM and mock embedder are fine — no third-party API keys needed on the episcience side.

## Step 5 — Register the MCP server with Claude Code

Every EpiScience MCP tool acts as the **authenticated caller**: the `agent_id` in the EpiGraph access token
that made the call. A stdio session carries no token, so over stdio the server can list its tools but refuses
every call. Run the MCP server on the HTTP transport instead:

```bash
DATABASE_URL=postgres://episcience_app:<app password>@localhost:5432/epigraph \
EPIGRAPH_JWT_SECRET=<your EpiGraph API's EPIGRAPH_JWT_SECRET> \
EPISCIENCE_LISTEN=127.0.0.1:8093 \
  /home/youruser/episcience/target/release/episcience-mcp-server
```

Then either federate it into your EpiGraph MCP gateway (`EPIGRAPH_MCP_EXTENSIONS`), which forwards each
caller's own token, or register `http://127.0.0.1:8093/mcp` in `~/.mcp.json` as an HTTP server with an
`Authorization: Bearer <EpiGraph access token>` header. The token needs an `agent_id`, plus `claims:read`
for the read tools and `claims:write` for the write tools.

Replace `/home/youruser/episcience` with the absolute path you cloned to. The MCP server exposes eight tools — four read/synthesis tools and four ELN write tools at parity with the HTTP routes:

Read + synthesis:

- `synthesize` — enqueue a synthesis job over a natural-language query, optionally polling to completion.
- `recall_synthesis` — semantic search over completed syntheses the calling agent can read.
- `get_synthesis` — fetch a single synthesis by id.
- `list_syntheses` — list readable syntheses, most-recent first.

ELN writes (Phase 8 — surface parity with HTTP):

- `propose_protocol` — insert a versioned `protocols` row. `authored_by` is the authenticated caller.
- `add_observation` — insert a kernel claim + a `sample_claims` link to a sample the caller prepared, atomically. `agent_id` is the authenticated caller.
- `countersign` — append an Ed25519 countersignature to a claim. `signer_id` is the authenticated caller.
- `attach_blob` — upload a content-addressed blob via base64 (MCP cannot do multipart). `uploader_id` is the authenticated caller (attaching to a sample requires having prepared it); enforces `EPISCIENCE_MAX_UPLOAD_BYTES` on the decoded payload.

Every tool takes its identity from the caller's validated token server-side — MCP clients cannot act as another agent, and there is no server-wide service identity.

(Tool names confirmed in `crates/episcience-api/src/mcp/mod.rs`.)

The MCP server's environment should also carry the blob-storage config so `attach_blob` works:

```json
"env": {
  "DATABASE_URL": "postgres://episcience_app:<app password>@localhost:5432/epigraph",
  "EPISCIENCE_BLOB_DIR": "/var/lib/episcience/blobs",
  "EPISCIENCE_MAX_UPLOAD_BYTES": "26214400"
}
```

`EPISCIENCE_BLOB_DIR` is where content-addressed bytes land on disk (mirrors the HTTP server's value — both processes must agree). `EPISCIENCE_MAX_UPLOAD_BYTES` defaults to 25 MiB (26214400) on the MCP side; raise it if your ELN turns include larger raw-data attachments.

Restart Claude Code so it picks up the new server.

## Step 6 — First synthesis claim

Open Claude Code and ask:

> Use `mcp__episcience__synthesize` with `query="Verification that episcience is installed"` and `wait_for_completion=true`.

The synthesize tool takes a natural-language `query` (not a `content` + `source_claims` shape) — the worker discovers source claims by embedding the query and searching the kernel. With `wait_for_completion=true`, the call blocks until the synthesis reaches a terminal state (cap: 600s). Against a fresh, near-empty kernel the worker will write a synthesis row with a short narrative even when no claims match — the mock LLM is deterministic.

You should see a JSON response with a `synthesis_id` and `status: "complete"` (plus a `narrative` field). Then:

> Use `mcp__episcience__recall_synthesis` with `query="verification"`.

The synthesis you just wrote should appear in the result, paired with a cosine score.

If both calls return successfully, episcience is wired up end-to-end on top of the kernel.

## Common errors

| Symptom | Fix |
|---|---|
| `relation "claims" does not exist` during migration | The kernel schema isn't in this database. Run EpiGraph [Step 3](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/01-quickstart.md#step-3--migrations) first, then retry Step 2. The episcience migrations layer on top of the kernel, they don't bootstrap it. |
| `EpiScience tables exist but the ledger is empty` from `episcience-migrate run` | The tables were built by the old hand-applied files. Run `episcience-migrate adopt-baseline`; it records the baseline only when the live tables match the committed fingerprint, and prints the diff otherwise. |
| `refusing: DATABASE_URL is set` from `episcience-migrate` | Unset `DATABASE_URL` (the runtime DSN); the migrator reads only `EPISCIENCE_MIGRATION_DATABASE_URL`. |
| `Address already in use` on `8091` | Pick a different `EPISCIENCE_PORT`. Avoid `8080` (EpiGraph). |
| `<VARIABLE> is set: episcience-… refuses to start with a retired service-client or service-identity variable` | Unset it (`EPIGRAPH_CLIENT_ID`, `EPIGRAPH_CLIENT_SECRET`, `EPIGRAPH_SERVICE_TOKEN` or `EPIGRAPH_SERVICE_AGENT_ID`): nothing reads them any more. |
| `the session is privileged or switched` at boot | The DSN names a superuser (or a login that can reach one). Use the application logins of Step 3. |
| MCP tool not found / not callable | Wrong URL in `~/.mcp.json`, or Claude Code wasn't restarted after editing the file. |
| MCP tool call refused with `Unauthorized` | The session has no valid token (stdio, or a missing/expired bearer), or the token has no `agent_id` (`principal_required`). Use the HTTP transport with an EpiGraph access token. |
| MCP or REST call refused with `insufficient_scope` | The token lacks `claims:read` (reads) or `claims:write` (writes). |
| Synthesize call returns `status: "queued"` and never completes | Jobs are run by `episcience-worker` (Step 4), not by the API server. If the worker is not running (or refused to start: its log names why), jobs sit in `synthesis_jobs` indefinitely. |

## Tear-down

Episcience tables live in the same database as the kernel, so dropping the EpiGraph database (Tear-down in the EpiGraph quickstart) removes everything. If you want to wipe only the episcience layer while preserving the kernel, drop in this order (children before parents to satisfy FKs):

```sql
DROP TABLE IF EXISTS
  synthesis_provo_edges,
  synthesis_claim_membership,
  synthesis_shares,
  synthesis_staleness_events,
  synthesis_jobs,
  synthesis_embeddings,
  synthesis_clusters,
  syntheses,
  countersignatures,
  blobs,
  protocols,
  sample_claims,
  samples,
  episcience_worker_state
CASCADE;
DROP SCHEMA episcience_meta CASCADE;  -- EpiScience's ledger (holds nothing else)
```

`experiments` and `experiment_results` are kernel tables; leave them. The kernel's own `_sqlx_migrations` was never written by EpiScience — no cleanup needed there.

---

Once verification passes, the next thing to read is [`02-concepts-science.md`](02-concepts-science.md) — it walks through samples, protocols, blobs, countersignatures, synthesis claims, and the post-SciLink pipeline features (skills, verifier, novelty, refinement, protocol sections). For workflow-shaped recipes that exercise those features end-to-end, see [`05-workflows.md`](05-workflows.md). Term-level lookups go to [`04-glossary.md`](04-glossary.md).
