# The Experiment Filer

The Experiment Filer files each agent-run experiment from the run's contract files alone, with no hand
filing (backlog a4002bc2). It is a service in this repository, beside `episcience-worker`, in its own crate
`crates/episcience-filer`.

It is built in phases. **Only phase 0 exists.** Phase 0 is shadow mode: it reads, validates and journals
the plan it would execute, and sends nothing anywhere.

| Phase | Backlog | State |
|---|---|---|
| 0, shadow mode | 28dde255 | this document |
| 1, kernel writer | f376c374 | not built |
| 2, EpiScience records | b35c944e | not built |
| 3 instrument sessions, 4 notebook view | under a4002bc2 | not built |

## What phase 0 does

- Reads the run contract: a manifest (`MANIFEST.sha256` or `MANIFEST.b3`), `RUN.json`, `RESULT.json`.
- Validates each complete run directory and rejects one that lacks a contract file or whose manifest does
  not verify, with a typed reason per failure.
- Builds the filing plan for an accepted run: one record per write a later phase would make, each under a
  deterministic key (campaign, cell, event, content hash).
- Appends the outcome and the plan to an outbox journal: append-only JSONL, fsynced, one JSON line per
  record.

What it does not do: it holds no credential, opens no socket and no database connection, and writes
nothing under the watched root. The crate depends on no `episcience-api`, `episcience-db` or kernel crate.
Its only output is the journal file.

## What it reuses

Phase 0 calls nothing. The plan names the existing surface the later phases call, so they add a client,
not a route:

| Record (`event`) | Existing surface | Where |
|---|---|---|
| `campaign_sample` | `POST /api/v1/eln/workflow_runs` (`workflow_id`, `canonical_name`, `started_at`, `labels`, optional `prepared_by`) | `crates/episcience-api/src/routes/workflow_runs.rs` |
| `run_sample` | `POST /api/v1/eln/samples` with `parent_sample_id` | `crates/episcience-api/src/routes/samples.rs` (`create_sample`) |
| `host_observation` | `POST /api/v1/eln/samples/:id/observations` (`content`, `relationship`) | same file (`add_observation`); MCP `add_observation` in `src/mcp/observations.rs` |
| `blob/<path>` | `POST /api/v1/eln/blobs`, multipart (`file`, `sample_id`, `labels`, `properties`, optional `uploader_id`) | `crates/episcience-api/src/routes/blobs.rs` (`upload_blob`); MCP `attach_blob` in `src/mcp/blobs.rs` |
| `gate/<name>` | the kernel's `POST /api/v1/claims` | kernel repository, `epigraph-api` |
| `data_pointer` | none yet: pointer-only blobs are a0bb3631, dataset evidence is c8beb101 | - |

Three facts about that surface the later phases must plan for:

- `POST /api/v1/eln/workflow_runs` is not idempotent: every call inserts a new sample under a fresh id. A
  writer must look its key up before it writes.
- That route requires `workflow_id` (a UUID) and `started_at`. Phase 0 treats both as optional in
  `RUN.json`, and no contract file gives a campaign's start time.
- EpiScience hashes blobs with BLAKE3 (`BlobRepository::store`); campaign manifests are usually SHA-256.

## The contract as implemented

**No schema for the contract files existed** in this repository or in the kernel repository when phase 0
was written. The schema below is the minimum the backlog items imply. Every field name is an assumption
until the owner confirms it against a real run directory; each is marked `INVENTED` in
`crates/episcience-filer/src/contract.rs`. Unknown fields are ignored.

`RUN.json`

| Field | Required | Meaning |
|---|---|---|
| `campaign` | yes | The campaign. First part of every key. |
| `cell` | yes | The run's cell within the campaign. Second part of the key. |
| `workflow_id` | no | The EpiGraph workflow UUID, for the `workflow_runs` route. |
| `canonical_name` | no | The workflow's canonical name. Absent: `campaign` is used. |
| `step_id` | no | The pre-registered workflow step the gate claims link to. |
| `started_at` | no | When the run started, verbatim. |
| `data_root_uri` | no | Where the run's data lives. Becomes the gate claims' `source_url`. |
| `host` | no | Host or instrument readiness, free-form. Becomes an observation. |
| `small_files` | no | Relative paths of the small files to upload as blobs. Each must be in the manifest. |

`RESULT.json`

| Field | Required | Meaning |
|---|---|---|
| `status` | yes | How the run ended, verbatim. A failed run is filed like a successful one. |
| `finished_at` | no | When the run ended, verbatim. |
| `gates[].name` | yes | The gate's name, unique within the run. |
| `gates[].verdict` | yes | The verdict, verbatim. |
| `gates[].statement` | yes | The claim text. Filed word for word. |
| `gates[].step_id` | no | Overrides `RUN.json`'s `step_id` for this gate. |

Manifest

- One line per file, as `sha256sum` and `b3sum` print it: 64 hex digits, two spaces (or a space and `*`),
  the path. `MANIFEST.sha256` carries SHA-256 digests, `MANIFEST.b3` BLAKE3 digests. The escaped form
  (`\` prefix) and BSD-style tagged lines are refused as malformed.
- It verifies when every line parses, no path is listed twice, and every listed file exists, is a regular
  file and has the listed digest.
- A path that is absolute, has a `..` component, or resolves through a symlink to somewhere outside the
  run directory is refused. So is a contract file that is such a symlink.
- A file present but not listed is reported, and rejects the run unless the validator is told to allow it
  (`ValidatorOptions::allow_unlisted`, off by default). The manifest files are exempt; `RUN.json` and
  `RESULT.json` are not, so the manifest must list them.
- When both manifests are present, both are verified and `MANIFEST.sha256` is the primary.
- The run's content hash is the hash of the primary manifest file's own bytes, written
  `sha256:<hex>` or `blake3:<hex>`.

These manifest rules are also assumptions to confirm: that the manifest lists the two JSON files, that
the content hash is the hash of the manifest file, and the choice of primary.

## Completeness

The manifest is the completeness signal. It lists the digest of every other file, `RESULT.json` included,
so it is written last.

- A directory with a manifest is a complete run directory. It is validated and journaled.
- A directory with `RUN.json` and no manifest is still running. It is reported as pending and never
  journaled.
- The scan does not descend into a run directory, does not follow symlinks, and goes at most eight levels
  below the root.

A run that never writes a manifest stays pending for ever. That a host was lost is a fact from the
orchestrator's log, which phase 0 does not read (see "Not in phase 0").

A complete directory can still be caught half-copied, when a sync delivers the manifest first. It is
journaled as rejected, re-validated on every scan, and journaled as accepted by the scan that sees the
finished copy. An accepted run is skipped afterwards without re-hashing its data.

## The journal

Two kinds of line:

```json
{"type":"record","key_id":"<blake3 hex>","key":{"campaign":"c","cell":"a","event":"gate/G1","content_hash":"sha256:..."},"action":{"kind":"submit_gate_claim", "...": "..."}}
{"type":"run","run_id":"<blake3 hex>","run_dir":"c/a","manifest_hash":"sha256:...","outcome":"accepted","campaign":"c","cell":"a","rejections":[],"warnings":[],"records":["<key_id>", "..."]}
```

- A run is one write (its new record lines, then its run line) followed by `fsync`. The run line is the
  commit marker.
- A record is written once per `key_id`. A run line is written once per `run_id`, which covers the
  directory, its manifest hash and the outcome. Scanning an unchanged root again appends nothing, also
  after a restart.
- A torn last line, left by a crash, is dropped when the journal is opened: the file is truncated back to
  the last whole line. Records whose run line was lost stay, and the next scan appends only what is
  missing. A malformed line anywhere else refuses the open.
- No line carries a timestamp or an absolute path. The same run directories give the same bytes.
- One writer per journal file. The journal takes no lock.

The campaign sample's key has no cell and no content hash, so it is journaled once per campaign, from the
first run seen (runs are visited in sorted path order).

## Running it

```bash
EPISCIENCE_FILER_ROOT=/path/to/campaigns \
EPISCIENCE_FILER_JOURNAL=/var/lib/episcience-filer/outbox.jsonl \
  cargo run -p episcience-filer --bin episcience-filer
```

| Variable | Meaning |
|---|---|
| `EPISCIENCE_FILER_ROOT` | Required. The directory scanned for run directories. Read only. |
| `EPISCIENCE_FILER_JOURNAL` | Required. The journal file. Keep it outside the root, on durable storage. |
| `EPISCIENCE_FILER_POLL_SECS` | Optional. Seconds between scans. Unset: one scan, then exit. |

Each scan prints one JSON summary to stdout. Like every EpiScience binary (`docs/deploy.md`), it refuses
to start when a retired service-client or service-identity variable is set. It is not part of the deploy
procedure yet: phase 0 has no unit and no place in the promote step.

Tests: `cargo test -p episcience-filer`. They need no database.

## Not in phase 0

- **The orchestrator's logs.** The parent item has the watcher read orchestrator logs as well as run
  directories. Their format is defined nowhere, so phase 0 reads run directories only.
- **The workflow outcome at campaign close.** No contract file says a campaign has closed, so the plan has
  no outcome record.
- **The comparison with the hand-filed campaign.** 28dde255 is done when the plan generated from an
  already hand-filed campaign matches the hand-filed records. That needs the real run directories and has
  not been run.

## What the later phases add

Phase 1, the kernel writer (f376c374)

- A writer, the only part that holds a credential, files the journaled plan to the kernel over HTTP under
  the filer's own service agent. It needs c4cad28e, which specifies that identity. Note that
  `docs/deploy.md` has every binary refuse the retired service variables and states that no binary holds
  a kernel service credential, so c4cad28e has to define a new path, not revive the old one.
- Gate claims are filled only from `RESULT.json`: methodology `direct_observation`, evidence `empirical`,
  Document evidence with `source_url` = the data root URI and `checksum` = the manifest hash, moving to
  Dataset evidence once c8beb101 lands. Each is linked to the pre-registered workflow step.
- Every write carries its deterministic key and is looked up before it is written.
- A verifier re-hashes each manifest where the data lives before any pointer is filed. A corrupted file
  becomes a verify-mismatch observation.
- The workflow outcome is reported once, at campaign close.

Phase 2, EpiScience records (b35c944e)

- A `workflow_run` sample per campaign through the existing `workflow_runs` route, a child sample per run,
  observations for host or instrument readiness, and the small files `RUN.json` names as blobs over
  multipart.
- Bulk data is registered by pointer once a0bb3631 lands, and uploads stream once 50936136 lands.
- The records are cross-linked to the phase 1 gate claims.
