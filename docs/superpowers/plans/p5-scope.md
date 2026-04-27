# Task 0.5 (P5) — Service-class JWT scope and agent creation

Empirically validated against a dev epigraph-api instance built from `feat/phase0-integrated` (Tasks 0.2+0.3+0.4 merged) running on port 8090 against a schema-cloned `epigraph_dev_synthesis` Postgres database.

## Scope name for edge writes

**`edges:write`** — exact string.

Enforcement: `crates/epigraph-api/src/routes/edges.rs:582,778,844` calls `crate::middleware::scopes::check_scopes(auth, &["edges:write"])` at the entry of POST `/edges` (line 582) and inside the `edge.added` / `claim.superseded` / `edge.deleted` emission helpers (lines 778, 844 — added by Task 0.3, but the scope check predates them).

The same scope guards every state-changing edge route. No upstream change required.

## Service-agent registration

**Agent table.** Insert into `agents` with `agent_type = 'service'`. Required columns: `id` (UUID), `public_key` (BYTEA, 32 bytes — `agents_public_key_length` constraint), `display_name` (e.g., `'episcience-service'`), `agent_type = 'service'`, `role = 'custom'`, `state = 'active'`. The `episcience-service` agent is the canonical "edge author" recorded as `auth.agent_id` on synthesis-emitted PROV-O edges.

**oauth_clients row.** Not strictly required for the JWT to validate — the `client_type: "service"` claim is in-token only. If you want auditable client management plus `oauth/register.rs` lifecycle, register an `oauth_clients` row tied to the agent.

**Existing infrastructure supports `client_type: "service"` directly.** `oauth/register.rs:61-66` already validates the value against `human | service | agent` and treats `service` as a known type.

## JWT minting

JWT scheme: HS256 over `EPIGRAPH_JWT_SECRET` (env var). Production deployment leaves it unset → falls back to dev string `epigraph-dev-secret-change-in-production!!` (state.rs:437). Set `EPIGRAPH_JWT_SECRET` to a real secret before any deploy that holds non-dev data.

Claims structure (verified against `crates/epigraph-api/src/oauth/jwt.rs:13-37`):

```json
{
  "sub":          "<oauth_clients.id UUID>",
  "iss":          "epigraph",
  "aud":          "epigraph-api",
  "exp":          <unix-seconds, +1y for service tokens>,
  "iat":          <unix-seconds>,
  "nbf":          <unix-seconds>,
  "jti":          "<unique UUID>",
  "scopes":       ["edges:write", "claims:read"],
  "client_type":  "service",
  "owner_id":     null,
  "agent_id":     "<agents.id UUID — episcience-service>"
}
```

Reference Python minter (used during P5 validation; not committed):

```python
import hmac, hashlib, base64, json, time, uuid, os

def b64url(b): return base64.urlsafe_b64encode(b).rstrip(b"=").decode()
secret  = os.environ.get("EPIGRAPH_JWT_SECRET",
                         "epigraph-dev-secret-change-in-production!!").encode()
now     = int(time.time())
ttl     = 3600 * 24 * 365  # 1 year
claims  = { "sub": str(uuid.uuid4()), "iss": "epigraph", "aud": "epigraph-api",
            "exp": now+ttl, "iat": now, "nbf": now, "jti": str(uuid.uuid4()),
            "scopes": ["edges:write","claims:read"], "client_type": "service",
            "owner_id": None, "agent_id": str(uuid.uuid4()) }
header  = {"alg":"HS256","typ":"JWT"}
hdr_b   = b64url(json.dumps(header, separators=(",", ":")).encode())
pld_b   = b64url(json.dumps(claims, separators=(",", ":")).encode())
sig     = hmac.new(secret, f"{hdr_b}.{pld_b}".encode(), hashlib.sha256).digest()
print(f"{hdr_b}.{pld_b}.{b64url(sig)}")
```

Production minting should use `JwtConfig::issue_access_token` from a tiny CLI binary; the Python script above is for ad-hoc dev testing only.

The minted token belongs in `~/.episcience/service-token.env` as
`EPIGRAPH_SERVICE_TOKEN=<token>` and `EPIGRAPH_SERVICE_AGENT_ID=<agent_uuid>`.
**Never commit this file.**

## GATE 0.5 smoke-test results (executed 2026-04-27)

Setup:
- Schema-clone of prod DB into `epigraph_dev_synthesis` (`pg_dump --schema-only` then load).
- Built `target/release/server` from worktree on `feat/phase0-integrated`.
- Ran with `DATABASE_URL=postgres://...:5432/epigraph_dev_synthesis EPIGRAPH_PORT=8090`.
- Minted service JWT with the Python minter above.
- Seeded `episcience-service` agent + two test claims directly via `psql` (test-only).

| Test | Request | Expected | Actual |
|---|---|---|---|
| Synthesis source w/ valid token | `POST /edges` `{source_type:"synthesis", source_id:<placeholder>, target_type:"claim", target_id:<placeholder>, relationship:"WAS_DERIVED_FROM"}` | non-401/403 | **HTTP 404** with `{"error":"NotFound","message":"synthesis with ID … not found"}` — entity types accepted, scope passed, handler tried entity lookup |
| Missing auth header | `POST /edges` (no `Authorization`) | 401 | **HTTP 401** ✓ |
| Bad relationship | `POST /edges` `{… relationship:"NONSENSE_PREDICATE"}` | 400/422 | **HTTP 400 ValidationError** with full valid list including `WAS_DERIVED_FROM, REFINES, COMPOSED_OF, METHODOLOGY, SUPERSEDES` ✓ |
| Bad source_type | `POST /edges` `{source_type:"synthesizer", …}` | 400/422 | **HTTP 400 ValidationError** with full valid list including `synthesis` ✓ |
| Real claim→claim SUPPORTS edge | `POST /edges` between two seeded claims | 201 | **HTTP 201** with edge ID ✓ |

The plan said "201 + edge id, OR 422 if synthesis-id is unknown". Actual upstream behavior for an unknown referenced entity is **404**, not 422. The semantic is identical (entity lookup fails after passing validation+auth); only the status code differs. Plan reads "non-401/403" — satisfied.

## Task 0.3 emission verification (caveat)

`POST /edges` succeeded (HTTP 201) but the `edge.added` event was NOT visible via `GET /api/v1/events`. Cause: pre-existing architectural split in epigraph — Task 0.3 emits to `super::events::global_event_store()` (in-memory `EventStore` singleton), while `GET /api/v1/events` with `feature = "db"` (the production default) reads from `epigraph_db::EventRepository` (Postgres `events` table). The two are independent stores. Task 0.3's unit tests verify the in-memory emission path; full HTTP-stack verification is recorded as a Phase 4 must-include in `p3-status.md`.

This is not a Task 0.3 defect — the spec target was the in-memory bus, which is what `StalenessWorker` will subscribe to in Phase 4. Re-running the same handler-emit→bus-listen path against `feature = "db"` requires the bus impl to dual-write or for the events-route to drain both stores; that decision belongs upstream and is out of scope for this PR sequence.

## GATE 0.5 status

**Investigation: complete.** Scope name confirmed (`edges:write`), agent-registration path documented, JWT minting recipe verified end-to-end, smoke tests run against a real built API and a schema-cloned database. No upstream changes needed for P5 — episcience can proceed with the existing scope and JWT infrastructure.
