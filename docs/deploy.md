# Deploying EpiScience

The EpiScience binaries run from `/usr/local/bin`, installed by `root`, mirroring the
EpiGraph convention documented in `epigraph/docs/deploy.md`. Cargo's build output directory
is a **build cache, not a deploy target** — nothing in production runs out of it.

| Binary | systemd unit | Listens on |
|---|---|---|
| `/usr/local/bin/episcience-server` | `episcience.service` | `127.0.0.1:8092` (ELN; `EPISCIENCE_BIND_ADDR`:`EPISCIENCE_PORT`) |
| `/usr/local/bin/episcience-mcp-server` | `episcience-mcp.service` | `127.0.0.1:8093` (federated by `epigraph-mcp`) |
| `/usr/local/bin/episcience-worker` | `episcience-worker.service` | nothing (the synthesis queue, on the `episcience_worker` login) |
| `/usr/local/bin/episcience-maint` | `episcience-maint.service` + `episcience-maint.timer` (every 2 min, `tick`) | nothing (the `episcience_maint` login) |

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
| `DATABASE_URL` | required | required | The `episcience_app` application login (a member of `epigraph_app` and `episcience_rw`), from the unit's `EnvironmentFile`: neither binary reads a `.env` file. A superuser, BYPASSRLS or maintenance-member login, or a role switch in the DSN, refuses boot. |
| `EPIGRAPH_SESSION_GUC_MODE` | optional | optional | `transaction` behind a transaction-mode pooler (the boot probe proves the choice). |
| `EPIGRAPH_JWT_SECRET` | required | required | The kernel's token secret. **No fallback**: both binaries exit non-zero at boot without it, when it is shorter than 32 bytes, or when it is the kernel's committed development literal (the kernel's own `assert_production_secret` rule). |
| `EPISCIENCE_BIND_ADDR` | optional | - | Default `127.0.0.1`; every wildcard spelling refused. |
| `EPISCIENCE_PORT` | optional | - | Default `8081`. |
| `EPISCIENCE_LISTEN` | - | optional | Unset = stdio. `<IP>:port`, `localhost:port` or `unix:/path` = streamable HTTP; wildcards refused. |
| `EPISCIENCE_INPROCESS_WORKER` | retired | - | The in-process runner is deleted (`episcience-worker` is the only runner). Unset, empty or `0/false/off` is a harmless leftover, warned about at boot (remove it); `1/true/on`, or any other value, asks for the retired runner and refuses the server's boot. |
| `EPISCIENCE_BLOB_DIR`, `EPISCIENCE_MAX_UPLOAD_BYTES` | optional | optional | Both processes must agree on the blob directory. |
| `EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP` | - | dev only | Mutually exclusive with `EPIGRAPH_JWT_SECRET`; loopback or unix listener only. The server can initialize and list tools; **every `tools/call` is refused**. |

Refused by the request servers (boot exits non-zero, naming the variable, even when empty):
`MAINTENANCE_DATABASE_URL`, `EPISCIENCE_MIGRATION_DATABASE_URL`, `EPISCIENCE_MAINT_DATABASE_URL` and
`EPISCIENCE_WORKER_DATABASE_URL`. No privileged DSN, and no other EpiScience login's DSN, belongs in a
request-serving process's environment.

Refused by EVERY EpiScience binary (server, MCP, worker, maint, migrate), first, before anything else is
checked (boot exits non-zero, naming the variable, even when empty or not UTF-8): the retired service
client and service identity, `EPIGRAPH_CLIENT_ID`, `EPIGRAPH_CLIENT_SECRET`, `EPIGRAPH_SERVICE_TOKEN` and
`EPIGRAPH_SERVICE_AGENT_ID`. Nothing reads them: every write acts as the calling principal (requests) or
the job's principal (the worker), stage 6 writes the kernel PROV edges and events in process on the
synthesis owner's transaction, and no binary holds a kernel service credential.

Ignored with a warning at boot (remove them): `EPISCIENCE_INPROCESS_WORKER` when off, `EPIGRAPH_API_URL`. No longer
read: `EPIGRAPH_JWT_AUDIENCE` (validation is fixed, see below).

### Environment files and units

Each unit reads exactly one root-owned 0600 environment file of its own, holding its own login's DSN and
nothing privileged; no unit loads the checkout's `.env`:

| Unit | Environment file (example path) | DSN variable (login) |
|---|---|---|
| `episcience.service` | `/etc/episcience/server.env` | `DATABASE_URL` (`episcience_app`) |
| `episcience-mcp.service` | `/etc/episcience/mcp.env` | `DATABASE_URL` (`episcience_app`) |
| `episcience-worker.service` | `/etc/episcience/worker.env` | `EPISCIENCE_WORKER_DATABASE_URL` (`episcience_worker`) |
| `episcience-maint.service` (+ `.timer`) | `/etc/episcience/maint.env` | `EPISCIENCE_MAINT_DATABASE_URL` (`episcience_maint`) |
| none: `episcience-migrate`, run by the operator at deploy time | an operator-held 0600 file, never a unit's | `EPISCIENCE_MIGRATION_DATABASE_URL` (the migration owner) |

The token secret (`EPIGRAPH_JWT_SECRET`) belongs only in the server's and the MCP server's files. A secret
rotation writes those two files and restarts both units.

### `episcience-worker` (names only)

| Variable | Notes |
|---|---|
| `EPISCIENCE_WORKER_DATABASE_URL` | Required, and the ONLY DSN it reads: the `episcience_worker` login (a member of `epigraph_app`, `episcience_rw`, `episcience_queue`). No `.env` file is read. |
| `EPIGRAPH_SESSION_GUC_MODE` | Optional; `transaction` behind a transaction-mode pooler (the boot probe proves the choice). |
| `EPISCIENCE_LLM_MODE`, `ANTHROPIC_API_KEY`, `ANTHROPIC_MODEL`, `EPISCIENCE_EMBED_MODE`, `OPENAI_API_KEY`, `EPISCIENCE_EMBEDDING_MODEL`, `EPISCIENCE_COST_BUDGET` | The synthesis model providers (the server reads only the embedder ones, for its search route; use the same values). |

It refuses to start when any of `MAINTENANCE_DATABASE_URL`, `EPISCIENCE_MIGRATION_DATABASE_URL`,
`EPISCIENCE_MAINT_DATABASE_URL`, `DATABASE_URL` or a retired service variable (above) is set (even empty,
or to a value that is not UTF-8), and
on a privileged or switched session: a role switch at connect time (`options=-c role=…`, a per-role default),
or a login from which a superuser, a BYPASSRLS role or the kernel maintenance role is reachable by
membership. `episcience-maint` applies the same check. Its sessions carry `application_name=episcience-worker`. It stops between jobs on
SIGTERM, so the unit's `TimeoutStopSec` must cover one synthesis; a job killed mid-stage stays `running`
(the queue never picks a running job up again) until an operator puts it back.

### `episcience-maint` (names only)

`EPISCIENCE_MAINT_DATABASE_URL` only (the `episcience_maint` login); it refuses `DATABASE_URL`, the
migration, kernel-maintenance and worker DSN variables and the retired service variables. `episcience-maint tick` runs the
narrowing sweep and then the blocked-row check: exit 0 when nothing is blocked, **exit 3** when the sweep
could not narrow a row (each named on stderr; each audited as `episcience.maint.sweep_blocked`). Treat exit
3 as an alert (the unit's `OnFailure=` hook); it repeats every run until an operator remedies the row: for a
public sample blocked by another owner's public child, a privileged session detaches the child
(`parent_sample_id = NULL`) or re-owns it; the next tick then narrows the sample.

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

The schema is applied ONLY by `episcience-migrate` (never by hand with `psql`: its ledger,
`episcience_meta._sqlx_migrations`, records every version with its checksum, and `verify` is the deploy
guard). The files under `migrations/legacy/` are history and run by nothing.

```bash
cd <checkout>
env CARGO_TARGET_DIR=<target dir> CARGO_BUILD_JOBS=2 SQLX_OFFLINE=true \
    nice -n 10 cargo build --release --locked --bin episcience-server --bin episcience-mcp-server \
    --bin episcience-migrate --bin episcience-maint --bin episcience-worker

# Schema. episcience-migrate reads ONLY EPISCIENCE_MIGRATION_DATABASE_URL (the
# migration credential, never a runtime one) and refuses while DATABASE_URL is set.
env -u DATABASE_URL EPISCIENCE_MIGRATION_DATABASE_URL=... \
    <target dir>/release/episcience-migrate run
env -u DATABASE_URL EPISCIENCE_MIGRATION_DATABASE_URL=... \
    <target dir>/release/episcience-migrate verify   # non-zero = stop

# Promote. This install step is REQUIRED — a rebuild alone changes nothing in production.
for b in episcience-server episcience-mcp-server episcience-migrate episcience-maint episcience-worker; do
    sudo -n install -m 0755 <target dir>/release/$b /usr/local/bin/$b
done

# The MCP server first, then the server, then the worker (the maintenance timer
# picks up its binary at its next tick).
sudo -n systemctl restart episcience-mcp episcience episcience-worker
```

`CARGO_BUILD_JOBS=2` and `nice` are deliberate: this host has 7.6GB RAM and builds have OOMed
the running prod services. Keep them.

## Verify

```bash
systemctl is-active episcience episcience-mcp episcience-worker
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
function), the EpiScience schema was not migrated before the binaries were installed (item `S1`),
or the database login the process uses does not inherit the kernel application role's privileges
(item `S2`: a missing `GRANT epigraph_app TO <login>`, or a NOINHERIT grant); fix that, never the
check.

## Tenancy columns (5034, 5035): an expand, a data step, a contract

The ownership pair is added nullable (5034), legacy rows are re-owned by an
audited one-shot, then the pair becomes mandatory (5035). The order is
strict; each step is a checkpoint:

```sh
episcience-migrate run --to 5034                 # expand (nothing enforced)
# install the new server + MCP binaries (they declare every pair they write)
episcience-maint backfill-owners --principal <operator principal> --dry-run \
    --manifest <private path>/backfill-<ts>.json [--expect-group <group>]
# review the manifest, then:
episcience-maint backfill-owners --principal <operator principal> --apply \
    --manifest <private path>/backfill-applied-<ts>.json [--expect-group <group>]
episcience-migrate run                            # contract (5035)
episcience-migrate verify
```

`episcience-maint` reads only `EPISCIENCE_MAINT_DATABASE_URL` (the
`episcience_maint` login, which holds no table privilege: the maintenance-owned
definers are its whole authority), refuses a superuser / BYPASSRLS /
kernel-maintenance session and any other DSN variable. The principal is always
an argument. The new binaries refuse to boot on a schema without the tenancy
columns (they run on 5034 alone, which the order above needs).

Rollback, in this order (the previous binary cannot decode the `group`
vocabulary, and one undecodable row fails a whole list):

1. stop the new server and MCP units;
2. if 5035 was applied: `docs/runbooks/5035-undo.sql` (compensating SQL);
3. optionally `episcience-maint backfill-owners --reverse <applied manifest>`
   (it accepts only the manifest an applied run recorded);
4. `docs/runbooks/e1c-rollback-vocabulary.sql` (`group` -> `private` for
   syntheses). It first prints how many `group` rows sit in samples,
   protocols, blobs and countersignatures: the previous binary reads those
   tables without an ownership filter, so every such row becomes readable by
   every token holder. Decide on them (delete, keep, or do not roll back)
   before step 5;
5. install and start the previous binaries.

## Row security and the definer set (5036, 5037)

```sh
episcience-migrate run                        # 5036 (row security, policies, grants), 5037 (definers)
episcience-migrate backfill-signature-hashes  # link hashes of countersignatures written without one
episcience-migrate verify                     # must exit 0
# install the new binaries and restart, then:
episcience-migrate backfill-signature-hashes  # rows the previous binary wrote in between
episcience-migrate verify                     # must exit 0
```

`verify` saying `countersignature(s) carry no link hash` means: run
`backfill-signature-hashes`, then `verify` again. A link hash that `is not the
hash of its signature`, or one that `chains on a hash no countersignature of
its claim carries`, is a stop: a writer stored a wrong link; investigate
before going on. Run the backfill and `verify` again after any
`e1e-undo.sql` / `episcience-migrate run` cycle (the previous binary writes
no link hash).

Nothing changes for the running processes (they are still privileged); the
kernel application role loses write access to the EpiScience tables. Both
migrations set a 5 s lock timeout: if a long transaction holds one of the
tables, `run` fails with nothing applied; run it again (or stop the
EpiScience units for the step).

If `verify` names a grantee outside the matrix (a login that held privileges
on these tables before, through default privileges), first find out whether
anything uses that login (a report, an export, a non-superuser logical dump:
under forced row security such a login already reads only public rows, and a
non-superuser dump of these tables fails), decide per grantee, then revoke
its privileges on the named tables as the migration owner and run `verify`
again: any login with a table privilege could stamp any group.

Rollback, while every EpiScience process still runs on the privileged
connection: `docs/runbooks/episcience-rls-undo.sql` (row security off, the
pre-5036 grants back; `verify` refuses while it is in effect), and
`docs/runbooks/episcience-rls-redo.sql` to re-apply. To roll back further
than 5036: `docs/runbooks/e1e-undo.sql` (5037 and 5036 removed, their
ledger rows too; `episcience-migrate run` re-applies them), then
`docs/runbooks/5035-undo.sql`, which refuses (changing nothing) while E1e is
recorded, and when the data holds a row a re-apply of 5035 would refuse: roll
forward in that case.

## The worker split (5038, 5039)

```sh
episcience-migrate run      # 5038 (the insert-time signature-hash guard), 5039 (the blocked-row detector)
episcience-migrate verify   # must exit 0
# install the four binaries; install the worker and maintenance units DISABLED;
# set EPISCIENCE_INPROCESS_WORKER=0 for the server and restart it; then
systemctl enable --now episcience-worker.service episcience-maint.timer
# remove EPIGRAPH_CLIENT_ID / EPIGRAPH_CLIENT_SECRET from every EpiScience environment
# and restart the server and the MCP server
```

(Historical: from the cleanup batch the in-process runner is gone (its switch is only judged: `0` is a
warned leftover, `1` refuses the server's boot), and the retired client variables refuse boot; see "The
detach and cleanup" below.)

From here the synthesis queue, the stage-6 outbox retries and the staleness rechecks run in
`episcience-worker`, each synthesis stamped as its own principal (`synthesis_jobs.principal_id`), and the
maintenance timer narrows what stopped being publishable. Rollback (with the worker-split server binary
only: from the application-login switch on, the server has no in-process runner and refuses
`EPISCIENCE_INPROCESS_WORKER=1`): stop the worker and the timer, set `EPISCIENCE_INPROCESS_WORKER=1` (or
unset) and restart the server; further back,
`docs/runbooks/e1f-undo.sql` (the worker and the timer stopped) removes 5038 and 5039 and their ledger rows,
and `docs/runbooks/e1e-undo.sql` refuses until it has run.

## The application-login switch (no migration)

```sh
# the server and the MCP server move to their own environment files, each holding the
# episcience_app DSN as DATABASE_URL, EPIGRAPH_JWT_SECRET, and the non-secret settings
# above (blob directory, bind/port or listener, the embedder variables the search route
# uses, the upload cap); no MAINTENANCE_DATABASE_URL, no migration DSN, no worker or maintenance
# login DSN, no client variables
# install both binaries; restart the MCP server first, then the server
```

Every request now runs on a session stamped as its caller (see `docs/tenancy-contract.md`,
"The request path"). Rollback: point the units back at the previous environment and the previous
binaries; row security stays installed, and the previous binaries' privileged sessions bypass it, so a
rollback never opens more than before the switch.

## The detach and cleanup (5040)

```sh
episcience-migrate run      # 5040: drops the legacy edges_shared_evidence trigger on the kernel's
                            # edges table and its function (a no-op where they never existed)
episcience-migrate verify   # must exit 0
# before installing: remove every retired service variable (EPIGRAPH_CLIENT_ID,
# EPIGRAPH_CLIENT_SECRET, EPIGRAPH_SERVICE_TOKEN, EPIGRAPH_SERVICE_AGENT_ID) from every
# EpiScience environment file and unit (each now refuses boot), and the leftover
# EPISCIENCE_INPROCESS_WORKER (off: warned; asking for the runner: the server refuses)
# and EPIGRAPH_API_URL; names-only check afterwards
# install the five binaries; restart the MCP server, the server, the worker
```

5040 takes an exclusive lock on the kernel's `edges` table for the drop, with a 5 s lock timeout: on a
busy table `run` fails with nothing applied; run it again. Effect: the kernel no longer derives
`shared_evidence` factors from `analysis --provides_evidence--> claim` edges (EpiScience's legacy trigger
did); existing factor rows are untouched. Rollback, on an explicit decision only:
`docs/runbooks/5040-undo.sql` recreates the last legacy definition and un-records 5040 (it refuses unless
5040 is recorded, and while either object exists). The previous binaries need none of the retired
variables: their server and MCP server warn about them and their worker refuses the client ones, so the
environment cleaned for this step boots them unchanged.

## Why the binary is not run from the cargo target directory

Until 2026-08-02 `episcience.service` had `ExecStart=/home/jeremy/.cargo-target/release/episcience-server`,
running straight out of the shared build cache. That coupled a *disk-cleanup* concern to a *prod-uptime*
concern: `cargo clean`, a `CARGO_TARGET_DIR` change, or pruning build artifacts would have deleted the
live service's ExecStart, breaking EpiScience on its next restart (the running process survives via its
open inode, so the breakage surfaces later and looks unrelated). `/home/jeremy/.cargo-target` is also the
*shared* deploy target for EpiGraph builds, so unrelated work could have clobbered it.

Until the application-login switch both units read `EnvironmentFile=<checkout>/.env` (mode 600,
owned by the service user, written by the token-secret rotation script) with
`WorkingDirectory=<checkout>`. From the switch each unit has its own environment file holding the
application login (above); the unit drop-in resets `EnvironmentFile=` before naming the new file, so
the checkout's `.env` is no longer loaded at all, and the token-secret rotation must write the two new
files instead of the checkout's `.env`.
