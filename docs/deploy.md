# Deploying EpiScience

Both EpiScience binaries run from `/usr/local/bin`, installed by `root`, mirroring the
EpiGraph convention documented in `epigraph/docs/deploy.md`. Cargo's build output directory
is a **build cache, not a deploy target** — nothing in production runs out of it.

| Binary | systemd unit | Listens on |
|---|---|---|
| `/usr/local/bin/episcience-server` | `episcience.service` | `0.0.0.0:8092` (ELN) |
| `/usr/local/bin/episcience-mcp-server` | `episcience-mcp.service` | `127.0.0.1:8093` (federated by `epigraph-mcp`) |

## Build and promote

```bash
cd /home/jeremy/episcience
env CARGO_TARGET_DIR=/home/jeremy/.cargo-target CARGO_BUILD_JOBS=2 SQLX_OFFLINE=true \
    nice -n 10 cargo build --release --locked --bin episcience-server --bin episcience-mcp-server

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
curl -sS localhost:8092/health          # {"service":"episcience-eln","status":"healthy",...}
sudo -n ls -l /proc/$(systemctl show episcience -p MainPID --value)/exe   # must be /usr/local/bin/...
```

Unauthenticated `GET /` returns **401** — that is a healthy response, not a failure. Use `/health`
for an unauthenticated check.

## Why the binary is not run from the cargo target directory

Until 2026-08-02 `episcience.service` had `ExecStart=/home/jeremy/.cargo-target/release/episcience-server`,
running straight out of the shared build cache. That coupled a *disk-cleanup* concern to a *prod-uptime*
concern: `cargo clean`, a `CARGO_TARGET_DIR` change, or pruning build artifacts would have deleted the
live service's ExecStart, breaking EpiScience on its next restart (the running process survives via its
open inode, so the breakage surfaces later and looks unrelated). `/home/jeremy/.cargo-target` is also the
*shared* deploy target for EpiGraph builds, so unrelated work could have clobbered it.

Config and secrets are unchanged: both units read `EnvironmentFile=/home/jeremy/episcience/.env`
(mode 600, owned by `jeremy`, managed by the rotation script) with `WorkingDirectory=/home/jeremy/episcience`.
