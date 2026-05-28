# End-to-end workflows

Three concrete walkthroughs that exercise the post-SciLink-merge surface of episcience: a default synthesis through verifier accept, a verifier reject that anneals into a refinement chain, and a full ELN turn driven through MCP. Each walkthrough cites the routes, MCP tools, and SQL transitions a reader can verify on their own kernel.

These complement the conceptual walk of [`02-concepts-science.md`](02-concepts-science.md). If the concepts there are unfamiliar, read that first — every workflow here references the §-numbers from that file.

## Contents

1. [Workflow A — Default synthesis with verifier acceptance](#workflow-a--default-synthesis-with-verifier-acceptance)
2. [Workflow B — Refinement on verifier reject](#workflow-b--refinement-on-verifier-reject)
3. [Workflow C — End-to-end ELN turn through MCP](#workflow-c--end-to-end-eln-turn-through-mcp)

---

## Workflow A — Default synthesis with verifier acceptance

### Goal

Submit a synthesis query against a kernel with a small set of related claims, watch the worker drive the row through all seven stages (seed → traverse → cluster → narrate → compose → verify → novelty), and confirm the row settles in `complete` with a populated `verifier_outcome` and `novelty_score`. This is the happy-path baseline every other workflow contrasts against.

### Pre-conditions

- EpiGraph kernel running on `127.0.0.1:8080` and episcience API on `127.0.0.1:8091` (per [`01-quickstart-extension.md`](01-quickstart-extension.md)).
- All episcience migrations applied through `5025`. Verify: `psql ... -c "\d syntheses" | grep -E 'skill_name|verifier_outcome|novelty_score|refinement_temperature'` returns four lines.
- At least two related claims in the kernel that an embedding search over the query will surface. (A fresh kernel produces an empty cluster — the synthesis still completes with a short narrative against the mock LLM, but it's less illustrative.)

### Sequence

#### 1. Submit the synthesis

```bash
curl -s -X POST http://127.0.0.1:8091/api/v1/eln/syntheses \
  -H 'Content-Type: application/json' \
  -H 'X-Agent-Id: 0193a2c1-...-agent-uuid' \
  -d '{
    "query": "Yield observations for batch 12 origami tiles.",
    "visibility": "private"
  }'
```

Note: `skill_name` is omitted, so the row defaults to `"baseline"`. Response:

```json
{
  "id": "01964c00-aaaa-bbbb-cccc-dddddddddddd",
  "status": "queued"
}
```

The handler atomically inserts a `syntheses` row in `pending` and a `synthesis_jobs` row in `queued`, then returns `202 Accepted`. The HTTP request returns in milliseconds; the worker picks up the job out-of-band.

#### 2. Inspect the row immediately after submit

```sql
SELECT id, status, skill_name, verifier_outcome, novelty_score, narrative
  FROM syntheses
 WHERE id = '01964c00-aaaa-bbbb-cccc-dddddddddddd';
```

Expected (one row):

```
status           | pending
skill_name       | baseline
verifier_outcome | NULL
novelty_score    | NULL
narrative        | NULL
```

`pending` means the worker has not picked the row up yet. The job table has `state = 'queued'` for the same id.

#### 3. Watch the lifecycle

Re-run the same `SELECT` every second. The transitions you'll see (in order):

- `pending → running` — worker has picked the job up; stages 1–5 (seed → traverse → cluster → narrate → compose) are in flight.
- `running → verifying` — Stage 5 (Composition) finished; Stage 6 (Verification) is running.
- `verifying → complete` — Stage 6 returned `Accept`; the narrative is published, `synthesis_provo_edges` are materialised, and Stage 7 (Novelty) has run.

If the verifier rejects, the transition is `verifying → rejected` (and possibly a child appears — see Workflow B).

#### 4. Inspect the completed row

```sql
SELECT status, verifier_attempts,
       verifier_outcome,
       novelty_backend,
       novelty_score,
       length(narrative) AS narrative_chars
  FROM syntheses
 WHERE id = '01964c00-aaaa-bbbb-cccc-dddddddddddd';
```

Expected shape:

```
status            | complete
verifier_attempts | 1
verifier_outcome  | {"kind":"accept","rubric":"default_citation","evidence":{"cited_count":2}}
novelty_backend   | internal_prior_syntheses
novelty_score     | {"score":1.0,"backend":"internal_prior_syntheses","neighbours":[],"rationale":"no prior synthesis shares any cluster member"}
narrative_chars   | 412
```

On a fresh kernel with no prior syntheses sharing cluster members, the novelty score is exactly 1.0 with empty `neighbours` and the rationale `"no prior synthesis shares any cluster member"`. That is the right answer (concept §9): there is nothing to be redundant against.

#### 5. Confirm the PROV-O edges materialised

```sql
SELECT predicate, target_kind, target_id
  FROM synthesis_provo_edges
 WHERE synthesis_id = '01964c00-aaaa-bbbb-cccc-dddddddddddd'
 ORDER BY predicate;
```

Expected (illustrative — exact set depends on cluster contents):

- One or more `WAS_DERIVED_FROM` rows pointing at each kernel claim that contributed to the synthesis.
- One `ATTRIBUTED_TO` row pointing at the requesting agent.

(See concept §6 for the full predicate set and `target_kind` shape.)

### Common failure modes

| Symptom | Interpretation |
| --- | --- |
| Row stuck at `pending` for >10s | Worker not running. The synthesis-job runner is spawned by the API server itself; check the server logs and restart. |
| Row reaches `verifying` then `failed` (not `rejected`) | The Stage 6 call itself errored (DB read failure, LLM timeout during a stricter skill's verifier). Inspect `failure_reason` on the row. |
| `verifier_attempts = 1` but `status = 'rejected'` and no child row | Refinement was at the ceiling immediately (refinement_temperature `depth_delta` already 3) — possible only if the row was manually constructed. Default-submit rows always start at depth_delta 0. |
| `novelty_score` is `NULL` on a `complete` row | Novelty backend failed (e.g. embedder unavailable). Concept §9 documents this is non-fatal; the row still completes. |

---

## Workflow B — Refinement on verifier reject

### Goal

Engineer a synthesis whose generated narrative omits a cluster member (or hallucinates a citation), observe Stage 6 reject with `UncitedMember`, then watch the worker spawn a refinement child through the `REFINES` PROV-O edge. Walk the chain across three iterations and confirm it terminates at `depth_delta = 3`.

### Pre-conditions

- All of Workflow A's pre-conditions.
- An LLM mode that lets you produce a narrative-omitting-a-member. The simplest way is the mock LLM (`EPISCIENCE_LLM_MODE` unset; default behaviour) configured to return a narrative that cites only the first cluster member while the cluster has two — see the integration-test harness in `crates/episcience-api/tests/` for the canonical fixture.

### Sequence

#### 1. Submit a query that will cluster two claims

```bash
curl -s -X POST http://127.0.0.1:8091/api/v1/eln/syntheses \
  -H 'Content-Type: application/json' \
  -H 'X-Agent-Id: 0193a2c1-...-agent-uuid' \
  -d '{
    "query": "Two-claim cluster intended to under-cite.",
    "visibility": "private"
  }'
```

Capture the returned `id` as `$PARENT`.

#### 2. Wait for Stage 6 to reject

```sql
SELECT id, status, verifier_attempts,
       verifier_outcome->>'kind' AS kind,
       verifier_outcome->'reason' AS reason,
       refinement_temperature
  FROM syntheses
 WHERE id = $PARENT;
```

Expected once the worker drains:

```
status                  | rejected
verifier_attempts       | 1
kind                    | reject
reason                  | {"uncited_member": {"claim_id": "<the-omitted-claim-uuid>"}}
refinement_temperature  | {"depth_delta": 0, "relevance_prune_relax": 1.0, "allow_soft_verifier": false}
```

The parent row is **terminal**. It stays in `rejected` forever; the next attempt is a sibling row.

#### 3. Locate the refinement child

```sql
SELECT s.id AS child_id, s.status,
       s.refinement_temperature
  FROM synthesis_provo_edges e
  JOIN syntheses s ON s.id = e.synthesis_id
 WHERE e.target_id = $PARENT
   AND e.predicate = 'REFINES'
   AND e.target_kind = 'synthesis';
```

(`synthesis_provo_edges.synthesis_id` is the *child* synthesis that owns the edge; `target_id` is the parent it refines. See [`migrations/synthesis/5018_create_synthesis_provo_edges.sql`](../../migrations/synthesis/5018_create_synthesis_provo_edges.sql).)

Expected (assuming refinement is enabled — i.e. the parent's `depth_delta` was < 3):

```
child_id               | 01964c10-...
status                 | pending  (then running, verifying, ...)
refinement_temperature | {"depth_delta": 1, "relevance_prune_relax": 0.8, "allow_soft_verifier": true}
```

The child carries the **annealed** temperature: `depth_delta` advanced from 0 to 1, `relevance_prune_relax` multiplied by 0.8, `allow_soft_verifier` flipped to true. The child inherits the parent's `skill_name`.

#### 4. Watch the child run, and re-walk on each reject

If the second iteration also rejects, the same shape repeats — a grandchild row with `depth_delta = 2`. And again at `depth_delta = 3`. When the leaf at `depth_delta = 3` rejects, no further child is created — `RefinementTemperature::at_ceiling()` returns true and the worker writes the leaf as `rejected` and stops.

You can walk the whole chain with one recursive query:

```sql
WITH RECURSIVE chain AS (
    SELECT id, status, refinement_temperature, NULL::uuid AS parent_id
      FROM syntheses
     WHERE id = $PARENT
    UNION ALL
    SELECT s.id, s.status, s.refinement_temperature, e.target_id AS parent_id
      FROM chain c
      JOIN synthesis_provo_edges e
        ON e.target_id = c.id
       AND e.predicate = 'REFINES'
       AND e.target_kind = 'synthesis'
      JOIN syntheses s ON s.id = e.synthesis_id
)
SELECT id, status,
       (refinement_temperature->>'depth_delta')::int AS depth_delta
  FROM chain
 ORDER BY depth_delta;
```

A converged-on-failure chain looks like:

```
id          | status   | depth_delta
01964c00-.. | rejected | 0
01964c10-.. | rejected | 1
01964c20-.. | rejected | 2
01964c30-.. | rejected | 3   (terminal — no child)
```

A chain that found a valid narrative on the second attempt looks like:

```
id          | status    | depth_delta
01964c00-.. | rejected  | 0
01964c10-.. | complete  | 1   (leaf — verifier accepted with thawed traversal)
```

### Common failure modes

| Symptom | Interpretation |
| --- | --- |
| Parent in `rejected` but no child appears | Either refinement is disabled in this build (check the job handler) or the parent was already at the ceiling (manual insert with `depth_delta = 3`). |
| Child appears but stuck in `pending` indefinitely | The new job did not get enqueued. Check `synthesis_jobs` for a queued row with the child id; restart the API server if absent. |
| Chain converges to `complete` immediately on a thaw despite the original failure | The annealed traversal pulled in different cluster members, so the new cluster is what the LLM gets to cite. This is the intended outcome — refinement is supposed to vary the input, not just retry on the same one. |
| `allow_soft_verifier: true` but the chain still rejects with the same `UncitedMember` reason | The default rubric does not honour `allow_soft_verifier` today (concept §10) — it is a forward-compatible signal. Only skill-specific rubrics that opt in will downgrade. |

---

## Workflow C — End-to-end ELN turn through MCP

### Goal

Drive a complete ELN turn through MCP only — no HTTP, no `psql`. An agent (Claude Code or any MCP client) proposes a protocol, adds an observation, attaches a raw-data blob, runs a synthesis, and countersigns the resulting synthesis claim. At the end, the kernel + episcience tables hold one protocol + one sample + one observation claim + one blob + one synthesis row + one countersignature, all signed by the same MCP-authenticated agent.

This workflow exercises every MCP write tool added in the Phase 8 surface-parity work: `propose_protocol`, `add_observation`, `attach_blob`, `synthesize`, and `countersign`. (Plus the pre-existing query tools `recall_synthesis`, `get_synthesis`, `list_syntheses` for inspection.)

### Pre-conditions

- Episcience MCP server registered in `~/.mcp.json` per [`01-quickstart-extension.md`](01-quickstart-extension.md) Step 5.
- `EPISCIENCE_BLOB_DIR` and `EPISCIENCE_MAX_UPLOAD_BYTES` set on the MCP server's `env` block (the blob storage root and decoded-size ceiling). The MCP `attach_blob` tool enforces the ceiling on the base64-decoded payload.
- A pre-existing sample (`POST /api/v1/eln/samples` from the HTTP layer, or via a previous MCP turn) — the MCP surface today does **not** expose sample-creation directly; samples are created via HTTP. Capture the `sample_id` for this turn.
- A live EpiGraph agent id available — the MCP server is configured with a service `auth_agent_id` at startup (set on `EpiscienceServer::new`). All writes in this turn will be attributed to that agent; MCP clients cannot impersonate another agent.

### Sequence

#### 1. `propose_protocol`

In Claude Code:

> Use `mcp__episcience__propose_protocol` with `title="AFM scan of batch 12"`, `steps=[{"order": 1, "instruction": "Mount on mica."}, {"order": 2, "instruction": "Scan at 500nm window."}]`, `equipment=["afm-bruker-3"]`.

The tool inserts a `protocols` row with the MCP-authenticated agent as `authored_by`, computes the BLAKE3 `content_hash`, and returns `{ id, content_hash_hex }`. Capture the `id` as `$PROTOCOL`.

Expected DB transition:

```sql
SELECT id, title, version, authored_by FROM protocols WHERE id = $PROTOCOL;
-- version | 1   (root protocol; supersedes IS NULL)
```

#### 2. `add_observation`

> Use `mcp__episcience__add_observation` with `sample_id="<the-pre-existing-sample-uuid>"`, `content="AFM scan shows 87% well-formed tiles in batch 12."`, `relationship="measurement"`.

The tool inserts a kernel `claims` row (with `truth_value = 0.5`, neutral starting belief) plus a `sample_claims` link row, atomically. The claim's `agent_id` is the MCP-authenticated identity. Capture the returned `claim_id` as `$CLAIM`.

Expected DB transition:

```sql
SELECT c.id, c.truth_value, sc.relationship, sc.sample_id
  FROM claims c
  JOIN sample_claims sc ON sc.claim_id = c.id
 WHERE c.id = $CLAIM;
-- truth_value  | 0.5
-- relationship | measurement
```

#### 3. `attach_blob`

> Use `mcp__episcience__attach_blob` with `file_bytes_base64="<base64-encoded PNG>"`, `filename="batch-12-afm-001.png"`, `mime_type="image/png"`, `sample_id="<sample-uuid>"`.

The tool decodes the base64, enforces the `EPISCIENCE_MAX_UPLOAD_BYTES` ceiling on the decoded payload, computes the BLAKE3 hash, writes to the filesystem under `EPISCIENCE_BLOB_DIR/{hash[0:2]}/{hash[2:4]}/{hash}.blob`, and inserts the `blobs` row. Capture the `id` as `$BLOB`.

Expected DB transition:

```sql
SELECT id, content_hash, uploader_id, sample_id
  FROM blobs WHERE id = $BLOB;
-- uploader_id = MCP auth_agent_id
-- sample_id   = the sample_id passed in
```

The bytes on disk are deduplicated — re-uploading the same PNG produces a second `blobs` row but does not write a second file.

#### 4. `synthesize`

> Use `mcp__episcience__synthesize` with `query="What does batch 12 say about tile yield?"`, `wait_for_completion=true`.

The tool enqueues a synthesis job (a row in `syntheses` and a row in `synthesis_jobs`), polls until the row reaches a terminal state, and returns the final row. Capture the `synthesis_id` as `$SYNTH`. The synthesis runs the full pipeline including Stage 6 (verifier) and Stage 7 (novelty); the verifier-accept path is Workflow A.

The synthesis's narrative lives on the `syntheses` row itself — it is **not** republished as a fresh kernel `claims` row. What episcience writes back into EpiGraph at completion is a set of PROV-O edges (one `WAS_DERIVED_FROM` per cluster member, one `ATTRIBUTED_TO` per agent — see concept §6). That means there is no synthesis-derived kernel claim to countersign directly; what an agent typically countersigns at the end of an ELN turn is the **observation claim** (`$CLAIM` from step 2) — the underlying measurement the synthesis narrates over.

```sql
SELECT id, status, verifier_outcome->>'kind' AS verifier
  FROM syntheses WHERE id = $SYNTH;
-- status   | complete
-- verifier | accept
```

#### 5. `countersign`

Sign the observation claim from step 2 — the canonical message is `claim_id|signer_id|signature_meaning|content` where `content` is the claim's text. Sign with the MCP agent's Ed25519 private key (the agent's keypair is the responsibility of the MCP client, not the episcience server).

> Use `mcp__episcience__countersign` with `claim_id="$CLAIM"`, `signature_meaning="approved"`, `signature_hex="<128 hex chars>"`, `public_key_hex="<64 hex chars>"`.

The tool fetches the claim content, recomputes the canonical message, verifies the Ed25519 signature against the supplied public key, and only then inserts the `countersignatures` row. The `signer_id` is forced to the MCP-authenticated agent — clients cannot countersign on behalf of another agent.

Expected DB transition:

```sql
SELECT signer_id, signature_meaning, signature_version
  FROM countersignatures
 WHERE claim_id = $CLAIM
   AND signer_id = '<MCP auth_agent_id>'
   AND signature_meaning = 'approved';
-- signature_version | 2
```

(In a two-agent workflow — the more typical lab pattern — the *second* MCP-authenticated agent would countersign `$CLAIM` with `signature_meaning="reviewed"` or `"approved"`. The countersignature chain on a single claim is what implements the lab's "two-person rule" — see concept §5.)

### Final state

After all five tools have returned, the database holds:

| Table | Rows added | Linked by |
| --- | --- | --- |
| `protocols` | 1 (`$PROTOCOL`) | `authored_by = MCP agent` |
| `claims` (kernel) | 1 (`$CLAIM`, the observation) | `agent_id = MCP agent` |
| `sample_claims` | 1 | `claim_id = $CLAIM`, `sample_id = pre-existing sample` |
| `blobs` | 1 (`$BLOB`) | `uploader_id = MCP agent`, `sample_id = pre-existing sample` |
| `syntheses` | 1 (`$SYNTH`) | `agent_id = MCP agent`, status `complete`, narrative on the row itself |
| `synthesis_provo_edges` | ≥ 2 | one `WAS_DERIVED_FROM` per cluster member, one `ATTRIBUTED_TO` per agent |
| `countersignatures` | 1 | `claim_id = $CLAIM`, `signer_id = MCP agent`, `signature_meaning = approved` |

The full turn has produced one signed-and-witnessed observation claim, grounded in a sample, a protocol, and a raw-data blob, plus a synthesis claim that narrates over it — a complete ELN turn through one MCP client identity.

### Common failure modes

| Symptom | Interpretation |
| --- | --- |
| `propose_protocol` returns an error mentioning `authored_by` | The MCP `auth_agent_id` (set at server startup on `EpiscienceServer::new`) does not match the agent expected on the kernel side. Inspect the MCP server's `env` block. |
| `attach_blob` returns "payload too large" | The decoded (post-base64) size exceeded `EPISCIENCE_MAX_UPLOAD_BYTES`. Either chunk the blob or raise the ceiling. The ceiling defaults to 25 MiB on the MCP side. |
| `synthesize` returns `status: "rejected"` despite the data looking fine | The default verifier rejected — typically `UncitedMember` if the cluster contained more claims than the narrative cited. Workflow B walks the recovery path. |
| `countersign` returns "signature verify failed" | The Ed25519 signature did not match the canonical message `claim_id|signer_id|signature_meaning|content` with the supplied public key. Most likely: the client signed the raw `content` (signature_version 1 format) instead of the version-2 four-field string. The current MCP tool writes version 2. |
| `add_observation` errors with "sample not found" | The `sample_id` passed in does not exist (or was soft-deleted). Sample creation is HTTP-only today; create the sample via `POST /api/v1/eln/samples` first. |

---

Next step after these three workflows: read [`02-concepts-science.md`](02-concepts-science.md) §7–§11 for the conceptual underpinnings (skills, verifier, novelty, refinement, protocol sections). Term-level lookups go to [`04-glossary.md`](04-glossary.md).
