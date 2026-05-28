# Quickstart — episcience extension

This guide assumes you've completed the [EpiGraph quickstart](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/01-quickstart.md) and have a running kernel on `postgres://epigraph:epigraph@localhost/epigraph` with the API listening on `127.0.0.1:8080`.

Time budget: ~5 minutes if the kernel is already running.

## Prerequisites

- A completed EpiGraph quickstart (kernel migrations applied, API server running on `127.0.0.1:8080`)
- `psql` on your `$PATH` (you already have it from EpiGraph Step 1)

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

Episcience layers on top of the kernel schema that the EpiGraph quickstart already applied. Apply the episcience migrations directly with `psql` (the kernel's `cargo run --bin epigraph-migrate` and episcience's flat `migrations/001_initial_schema.sql` both use SQLx's `_sqlx_migrations` version `001`, so re-running through `sqlx migrate run --source migrations/` would trip a checksum mismatch — use `psql -f` instead, matching the production rollout pattern in `docs/superpowers/plans/2026-03-30-episcience-phase1.md`):

```bash
DATABASE_URL=postgres://epigraph:epigraph@localhost/epigraph

# Flat episcience migrations (experimental loop + signatures + samples +
# protocols + blobs + countersignatures + chain + protocol sections).
# 001 and 5001..5006 use IF NOT EXISTS guards; 5007..5010 do not — run
# each exactly once. 5025 (protocol sections) uses IF NOT EXISTS.
for f in migrations/001_initial_schema.sql \
         migrations/5001_signature_meaning.sql \
         migrations/5002_claims_fulltext_search.sql \
         migrations/5003_create_samples.sql \
         migrations/5004_create_protocols.sql \
         migrations/5005_create_blobs.sql \
         migrations/5006_create_countersignatures.sql \
         migrations/5007_quantity_pair_constraint.sql \
         migrations/5008_protocol_version_unique.sql \
         migrations/5009_samples_parent_restrict.sql \
         migrations/5010_countersign_chain.sql \
         migrations/5025_protocols_section_vocabulary.sql; do
  echo "=== Applying $f ==="
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f" || break
done

# Synthesis pipeline migrations (syntheses, jobs, embeddings, shares,
# membership, PROV-O edges, failure_reason, skill_name, verifier outcome,
# second skill, novelty, refinement temperature). 5011..5019 have no
# IF NOT EXISTS — one-shot, drop synthesis_* to re-run. 5020..5024 are
# ALTER TABLE columns and CHECK constraints; they tolerate re-application
# in the IF NOT EXISTS form but the CHECK-extension migrations (5021,
# 5022) drop-then-add and will fail cleanly if the prior version is
# missing.
for f in migrations/synthesis/5011_create_syntheses.sql \
         migrations/synthesis/5012_create_synthesis_clusters.sql \
         migrations/synthesis/5013_create_synthesis_embeddings.sql \
         migrations/synthesis/5014_create_synthesis_jobs.sql \
         migrations/synthesis/5015_create_synthesis_staleness_events.sql \
         migrations/synthesis/5016_create_synthesis_shares.sql \
         migrations/synthesis/5017_create_synthesis_claim_membership.sql \
         migrations/synthesis/5018_create_synthesis_provo_edges.sql \
         migrations/synthesis/5019_add_syntheses_failure_reason.sql \
         migrations/synthesis/5020_syntheses_skill_column.sql \
         migrations/synthesis/5021_syntheses_verifier_outcome.sql \
         migrations/synthesis/5022_syntheses_skill_lab_notebook.sql \
         migrations/synthesis/5023_syntheses_novelty.sql \
         migrations/synthesis/5024_syntheses_refinement_temperature.sql; do
  echo "=== Applying $f ==="
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f" || break
done
```

The episcience migrations depend on kernel functions and tables created by the EpiGraph kernel migrations (which you applied in EpiGraph Step 3). If you see a "function does not exist" or "relation does not exist" error mentioning a kernel symbol (e.g. `cascade_delete_edges`, `claims`, `frames`), the kernel migrations weren't applied first — go back to the [EpiGraph Step 3](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/01-quickstart.md#step-3--migrations).

> **Aside on `migrations/upstream/`.** The episcience repo also vendors a snapshot of the kernel migrations at `migrations/upstream/` (pinned to the same SHA as the `epigraph-*` workspace deps in `Cargo.toml`). That directory exists for the *fresh-database* bootstrap path — apply `upstream/001..016` first, then `5001..5010`, then `synthesis/5011..5019`, all from scratch — and for `cargo sqlx prepare` reproducibility. **For this quickstart, ignore it**: you already applied the kernel via the EpiGraph quickstart, and re-running `upstream/*.sql` over an existing kernel would `ALTER TABLE` against rows already in place.

## Step 3 — Build

```bash
cargo build --release -p episcience-api
```

This produces two binaries under `target/release/`:

- `episcience-server` — the HTTP API (`src/bin/server.rs`, registered as the `episcience-server` `[[bin]]` in `crates/episcience-api/Cargo.toml`).
- `episcience-mcp-server` — the MCP server for Claude Code (`src/bin/episcience-mcp-server.rs`).

## Step 4 — Start the API

```bash
export DATABASE_URL=postgres://epigraph:epigraph@localhost/epigraph
export EPISCIENCE_PORT=8091
export EPIGRAPH_API_URL=http://127.0.0.1:8080   # where EpiGraph's API is listening
# Optional but recommended — without it, the synthesis worker logs
# a 401 warning on every Stage-6 edge write back to EpiGraph:
# export EPIGRAPH_SERVICE_TOKEN=<token minted via scripts/mint_epigraph_token.py>

cargo run --release -p episcience-api --bin episcience-server
```

In another shell:

```bash
curl http://127.0.0.1:8091/health
```

Expected: an HTTP 200 with body `{"status":"healthy","service":"episcience-eln","version":"…"}`.

Notes on the env vars above:

- `EPISCIENCE_PORT` — port for the episcience HTTP server. Defaults to `8081` in `src/bin/server.rs`. We pick `8091` here so it doesn't collide with EpiGraph on `8080` or with the source's `EPIGRAPH_API_URL` default of `127.0.0.1:8090` (which is a default-for-prod-deploys quirk; for this quickstart we override it explicitly).
- `EPIGRAPH_API_URL` — where the synthesis worker writes PROV-O edges and where the staleness worker long-polls `/api/v1/events`. Must point at your EpiGraph API (Step 4 of the EpiGraph quickstart used `8080`).
- `EPIGRAPH_SERVICE_TOKEN` — used by the synthesis worker to authenticate to EpiGraph for edge writes. Without it the worker logs a warning at boot and Stage-6 edge writes return 401. The verification smoke below still works (the synthesis row completes and is searchable; only the cross-kernel edge writes are skipped), but production deployments must set this.

The server also accepts `EPISCIENCE_BLOB_DIR` (default `/var/lib/episcience/blobs`), `EPISCIENCE_MAX_UPLOAD_BYTES` (default 100 MB), `EPISCIENCE_LLM_MODE=anthropic` + `ANTHROPIC_API_KEY` (defaults to a mock LLM), and `EPISCIENCE_EMBED_MODE=openai` + `OPENAI_API_KEY` (defaults to a mock embedder). For the verification step below, the mock LLM and mock embedder are fine — no third-party API keys needed on the episcience side.

## Step 5 — Register the MCP server with Claude Code

Add an `episcience` entry to `~/.mcp.json` alongside the existing `epigraph` entry from the kernel quickstart:

```json
{
  "mcpServers": {
    "epigraph": { "...": "existing entry from EpiGraph quickstart" },
    "episcience": {
      "command": "/home/youruser/episcience/target/release/episcience-mcp-server",
      "env": {
        "DATABASE_URL": "postgres://epigraph:epigraph@localhost:5432/epigraph",
        "EPIGRAPH_API_URL": "http://127.0.0.1:8080"
      }
    }
  }
}
```

Replace `/home/youruser/episcience` with the absolute path you cloned to. The MCP server exposes eight tools — four read/synthesis tools and four ELN write tools at parity with the HTTP routes:

Read + synthesis:

- `synthesize` — enqueue a synthesis job over a natural-language query, optionally polling to completion.
- `recall_synthesis` — semantic search over completed syntheses the calling agent can read.
- `get_synthesis` — fetch a single synthesis by id.
- `list_syntheses` — list readable syntheses, most-recent first.

ELN writes (Phase 8 — surface parity with HTTP):

- `propose_protocol` — insert a versioned `protocols` row. `authored_by` is forced to the MCP-authenticated agent.
- `add_observation` — insert a kernel claim + a `sample_claims` link to an existing sample, atomically. `agent_id` is the MCP-authenticated agent.
- `countersign` — append an Ed25519 countersignature to a claim. `signer_id` is the MCP-authenticated agent.
- `attach_blob` — upload a content-addressed blob via base64 (MCP cannot do multipart). `uploader_id` is the MCP-authenticated agent; enforces `EPISCIENCE_MAX_UPLOAD_BYTES` on the decoded payload.

All four write tools enforce the MCP-server's `auth_agent_id` server-side — MCP clients cannot impersonate another agent. The `auth_agent_id` is set at server startup on `EpiscienceServer::new`; per-call JWT auth is a v2 concern.

(Tool names confirmed in `crates/episcience-api/src/mcp/mod.rs`.)

The MCP server's `env` block should also surface the blob-storage config so `attach_blob` works:

```json
"env": {
  "DATABASE_URL": "postgres://epigraph:epigraph@localhost:5432/epigraph",
  "EPIGRAPH_API_URL": "http://127.0.0.1:8080",
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
| `function "cascade_delete_edges" does not exist` (or similar) during migration | Kernel migrations not applied. Run EpiGraph [Step 3](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/01-quickstart.md#step-3--migrations) first, then retry Step 2. |
| `relation "claims" does not exist` during migration | Same root cause: kernel schema isn't in this database. The episcience migrations layer on top of the kernel, they don't bootstrap it. |
| `Address already in use` on `8091` | Pick a different `EPISCIENCE_PORT`. Avoid `8080` (EpiGraph) and `8090` (the source's `EPIGRAPH_API_URL` default — easy to confuse). |
| `EPIGRAPH_SERVICE_TOKEN not set — synthesis edge writes to <url> will fail with 401` at boot | Expected on a fresh dev box without service-token wiring. The synthesis row still completes; Stage-6 PROV-O edges back to the kernel won't land until you mint a token (see `scripts/mint_epigraph_token.py` in the EpiGraph repo). |
| MCP tool not found / not callable | Wrong absolute path in `~/.mcp.json`, or Claude Code wasn't restarted after editing the file. The `command` must be the full path to `target/release/episcience-mcp-server`, not a relative path. |
| `relation "<table>" already exists` on a synthesis migration re-run | The `synthesis/5011..5019` migrations have no `IF NOT EXISTS` guards. To re-apply, first `DROP TABLE` the offending table (or all of them: `syntheses, synthesis_jobs, synthesis_clusters, synthesis_embeddings, synthesis_staleness_events, synthesis_shares, synthesis_claim_membership, synthesis_provo_edges`) and re-run from `5011`. |
| `sqlx checksum mismatch` if you tried `sqlx migrate run --source migrations/` | Don't use `sqlx migrate run` for the episcience layer — the kernel's `_sqlx_migrations` already contains a row at `version=001` with a different checksum (the kernel's own `001_initial_schema.sql`), and sqlx-cli will refuse to proceed. Use the `psql -f` loop in Step 2 instead. |
| Synthesize call returns `status: "queued"` and never completes | The synthesis job runner is spawned by the API server itself (`src/bin/server.rs`). If the server crashed or wasn't started, jobs sit in `synthesis_jobs` indefinitely. Check the server logs and restart if needed. |

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
  experiment_results,
  experiments
CASCADE;
```

Step 2 used `psql -f` rather than `sqlx migrate run`, so `_sqlx_migrations` was never written to for these versions — no cleanup needed there.

---

Once verification passes, the next thing to read is [`02-concepts-science.md`](02-concepts-science.md) — it walks through samples, protocols, blobs, countersignatures, synthesis claims, and the post-SciLink pipeline features (skills, verifier, novelty, refinement, protocol sections). For workflow-shaped recipes that exercise those features end-to-end, see [`05-workflows.md`](05-workflows.md). Term-level lookups go to [`04-glossary.md`](04-glossary.md).
