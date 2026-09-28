# Deploying EpiScience

Both EpiScience binaries run from `/usr/local/bin`, installed by `root`, mirroring the
EpiGraph convention documented in `epigraph/docs/deploy.md`. Cargo's build output directory
is a **build cache, not a deploy target** — nothing in production runs out of it.

| Binary | systemd unit | Listens on |
|---|---|---|
| `/usr/local/bin/episcience-server` | `episcience.service` | `127.0.0.1:8092` (ELN; `EPISCIENCE_BIND_ADDR`:`EPISCIENCE_PORT`) |
| `/usr/local/bin/episcience-mcp-server` | `episcience-mcp.service` | `127.0.0.1:8093` (federated by `epigraph-mcp`) |

## Listen address

`episcience-server` listens on `EPISCIENCE_BIND_ADDR:EPISCIENCE_PORT`.

- `EPISCIENCE_BIND_ADDR` is an IP literal and defaults to `127.0.0.1`.
- Every spelling of the wildcard address (`0.0.0.0`, `::`, and the IPv4-mapped `::ffff:0.0.0.0`) is
  **refused at boot**: the process exits non-zero before it touches the database. The address is
  canonicalised first, so an IPv4-mapped literal is judged (and bound) as the IPv4 address it maps. A client
  that cannot use loopback gets the one specific address it needs (for example a bridge interface
  address), never every interface.
- `EPISCIENCE_PORT` defaults to `8081`; production sets `8092`.

The MCP server listens on `EPISCIENCE_LISTEN` (production: `127.0.0.1:8093`, set in the unit). The same
wildcard rule applies at boot. The value must be `<IP literal>:<port>` (IPv6 in brackets), `localhost:<port>`
or `unix:/abs/path`; any other host name is refused. With `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` only a
loopback address, `localhost` or a unix socket is accepted.

## Required environment (names only)

| Variable | Server | MCP | Notes |
|---|---|---|---|
| `DATABASE_URL` | required | required | |
| `EPIGRAPH_JWT_SECRET` | required | required | The kernel's token secret. **No fallback**: both binaries exit non-zero at boot without it, when it is shorter than 32 bytes, or when it is the kernel's committed development literal (the kernel's own `assert_production_secret` rule). |
| `EPISCIENCE_BIND_ADDR` | optional | - | Default `127.0.0.1`; every wildcard spelling refused. |
| `EPISCIENCE_PORT` | optional | - | Default `8081`. |
| `EPISCIENCE_LISTEN` | - | optional | Unset = stdio. `<IP>:port`, `localhost:port` or `unix:/path` = streamable HTTP; wildcards refused. |
| `EPIGRAPH_API_URL` | optional | optional | Kernel API base for stage-6 edge writes and event polling. |
| `EPIGRAPH_CLIENT_ID`, `EPIGRAPH_CLIENT_SECRET` | optional | optional | Kernel service credential for stage-6 edge writes and events (not a request identity). |
| `EPISCIENCE_BLOB_DIR`, `EPISCIENCE_MAX_UPLOAD_BYTES` | optional | optional | Both processes must agree on the blob directory. |
| `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` | - | dev only | Mutually exclusive with `EPIGRAPH_JWT_SECRET`; loopback or unix listener only. The server can initialize and list tools; **every `tools/call` is refused**. |

No longer read: `EPIGRAPH_JWT_AUDIENCE` (validation is fixed, see below) and `EPIGRAPH_SERVICE_AGENT_ID`
(MCP tools act as the authenticated caller; the MCP server logs a warning at boot if it is still set, so
remove it from the unit environment).

## Accepted tokens

Both surfaces accept only kernel-minted HS256 access tokens with `iss = "epigraph"`, `aud = "epigraph-api"`
and an unexpired `exp` (zero leeway).

- REST: the token must carry `agent_id` (else 401 `principal_required`). `GET`/`HEAD` need `claims:read`;
  every other method needs `claims:write` (else 403 `insufficient_scope`).
- MCP: every HTTP request needs a valid token, but `agent_id` is required only for `tools/call`, so the kernel
  gateway's discovery session (a principal-less service token) can still initialize and list tools. Read tools
  (`recall_synthesis`, `get_synthesis`, `list_syntheses`, `list_countersignatures`) need `claims:read`; write tools
  (`synthesize`, `propose_protocol`, `add_observation`, `countersign`, `attach_blob`) need `claims:write`.
  A stdio session has no token and can only list tools. An HTTP session is bound to the caller (OAuth client and
  agent) that opened it; another caller's token on that session id is answered as an unknown session.
- Every write is authored by the token's `agent_id`. A body field naming a different agent is refused, and a
  write that targets an existing sample requires the caller to have prepared it (404 otherwise). A new
  synthesis may name as parent or prerequisite only syntheses the caller can read (404 otherwise, the same
  answer as for an id that does not exist).

## Build and promote

Schema first, binaries second: the binaries refuse to start on a database whose
EpiScience schema is behind them (see "Tenancy contract" below).

```bash
cd /home/jeremy/episcience
env CARGO_TARGET_DIR=/home/jeremy/.cargo-target CARGO_BUILD_JOBS=2 SQLX_OFFLINE=true \
    nice -n 10 cargo build --release --locked --bin episcience-server --bin episcience-mcp-server \
    --bin episcience-migrate

# Schema. episcience-migrate reads ONLY EPISCIENCE_MIGRATION_DATABASE_URL (the
# migration credential, never a runtime one) and refuses while DATABASE_URL is set.
env -u DATABASE_URL EPISCIENCE_MIGRATION_DATABASE_URL=... \
    /home/jeremy/.cargo-target/release/episcience-migrate run
env -u DATABASE_URL EPISCIENCE_MIGRATION_DATABASE_URL=... \
    /home/jeremy/.cargo-target/release/episcience-migrate verify   # non-zero = stop

# Promote. This install step is REQUIRED — a rebuild alone changes nothing in production.
sudo -n install -m 0755 /home/jeremy/.cargo-target/release/episcience-server /usr/local/bin/episcience-server
sudo -n install -m 0755 /home/jeremy/.cargo-target/release/episcience-mcp-server /usr/local/bin/episcience-mcp-server

sudo -n systemctl restart episcience episcience-mcp
```

`CARGO_BUILD_JOBS=2` and `nice` are deliberate: this host has 7.6GB RAM and builds have OOMed
the running prod services. Keep them.

## Verify

```bash
systemctl is-active episcience episcience-mcp
curl -sS 127.0.0.1:8092/health         # {"service":"episcience-eln","status":"healthy",...}
ss -ltn '( sport = :8092 )'             # must show 127.0.0.1:8092 only (or the one address you configured)
sudo -n ls -l /proc/$(systemctl show episcience -p MainPID --value)/exe   # must be /usr/local/bin/...
```

Unauthenticated `GET /` returns **401** — that is a healthy response, not a failure. Use `/health`
for an unauthenticated check.

## Tenancy contract

Both binaries check tenancy contract v1 (`docs/tenancy-contract.md`) right after connecting to the
database and before serving. The journal then shows either `tenancy contract v1 probe OK` or
`tenancy contract v1 probe failed: <item>: expected …; …` followed by a non-zero exit. A refusal
means the kernel no longer provides an object EpiScience relies on (a revoked grant, a missing
function) or the EpiScience schema was not migrated before the binaries were installed; fix that,
never the check.

## Why the binary is not run from the cargo target directory

Until 2026-08-02 `episcience.service` had `ExecStart=/home/jeremy/.cargo-target/release/episcience-server`,
running straight out of the shared build cache. That coupled a *disk-cleanup* concern to a *prod-uptime*
concern: `cargo clean`, a `CARGO_TARGET_DIR` change, or pruning build artifacts would have deleted the
live service's ExecStart, breaking EpiScience on its next restart (the running process survives via its
open inode, so the breakage surfaces later and looks unrelated). `/home/jeremy/.cargo-target` is also the
*shared* deploy target for EpiGraph builds, so unrelated work could have clobbered it.

Config and secrets are unchanged: both units read `EnvironmentFile=/home/jeremy/episcience/.env`
(mode 600, owned by `jeremy`, managed by the rotation script) with `WorkingDirectory=/home/jeremy/episcience`.
