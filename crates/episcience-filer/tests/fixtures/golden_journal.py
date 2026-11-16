#!/usr/bin/env python3
"""Compute tests/fixtures/golden_journal.jsonl WITHOUT the filer crate.

The golden test (`tests/golden_journal.rs`) builds the run directories below,
scans them once and compares the journal byte for byte with this script's
output. This script re-implements the journal format from its description
(crates/episcience-filer/src/{journal,plan,manifest,validate}.rs docs and
docs/experiment-filer.md), with hashlib for SHA-256 and the `blake3` package
for BLAKE3; it never runs Rust. The file contents here and in the Rust test
must stay identical.

    python3 -m venv /tmp/venv && /tmp/venv/bin/pip install blake3
    /tmp/venv/bin/python -I golden_journal.py > golden_journal.jsonl

Ported from #72's Python-computed golden plan (backlog 28dde255), re-derived
for #71's journal format.
"""

import hashlib
import json
import struct
import sys

import blake3

STATEMENT_1 = (
    "Gate G1 PASS: the measured value 2.50 ± 0.10 is within the tolerance "
    "0.40 of the reference 2.60."
)

# run_dir -> (manifest algorithm or None, {path: bytes listed}, {path: bytes on disk if different})
RUNS = {
    "campaign-a/cell-01": (
        "sha256",
        {
            "RUN.json": (
                '{"campaign":"campaign-a","cell":"cell-01",'
                '"workflow_id":"00000000-0000-4000-8000-000000000001",'
                '"canonical_name":"example-workflow","step_id":"step-3",'
                '"started_at":"2026-01-02T03:04:05Z",'
                '"data_root_uri":"pvc://example-volume/campaign-a/cell-01",'
                '"host":{"driver":"1.2.3","gpu":"example-gpu"},'
                '"small_files":["logs/run.log"]}'
            ),
            "RESULT.json": (
                '{"status":"completed","finished_at":"2026-01-02T04:04:05Z",'
                '"gates":[{"name":"G1","verdict":"PASS","statement":"' + STATEMENT_1 + '"}]}'
            ),
            "logs/run.log": "step 1 ok\nstep 2 ok\n",
            "data/traj.csv": "t,x\n0,1.5\n1,0.5\n",
        },
        {},
    ),
    "campaign-a/cell-02": (
        "blake3",
        {
            "RUN.json": '{"campaign":"campaign-a","cell":"cell-02","step_id":"step-3"}',
            "RESULT.json": (
                '{"status":"failed","gates":['
                '{"name":"G1","verdict":"FAIL","statement":"Gate G1 FAIL: the run did not converge.","step_id":"step-9"},'
                '{"name":"G2","verdict":"PASS","statement":"Gate G2 PASS: the trajectory is complete."}]}'
            ),
            "data/traj.csv": "t,x\n0,9.0\n",
        },
        {},
    ),
    # Listed with one content, holding another: a rejection.
    "campaign-a/cell-03": (
        "sha256",
        {
            "RUN.json": '{"campaign":"campaign-a","cell":"cell-03"}',
            "RESULT.json": '{"status":"completed"}',
            "data/traj.csv": "t,x\n0,1.0\n",
        },
        {"data/traj.csv": "t,x\n0,2.0\n"},
    ),
    # RUN.json and no manifest: still running, never journaled (#71).
    "campaign-a/cell-04": (None, {"RUN.json": '{"campaign":"campaign-a","cell":"cell-04"}'}, {}),
    "campaign-b/cell-01": (
        "sha256",
        {
            "RUN.json": '{"campaign":"campaign-b","cell":"cell-01"}',
            "RESULT.json": '{"status":"completed","gates":[]}',
        },
        {},
    ),
}

KEY_DOMAIN = "episcience-filer/v1/key"
RUN_DOMAIN = "episcience-filer/v1/run"
MANIFEST_FILE = {"sha256": "MANIFEST.sha256", "blake3": "MANIFEST.b3"}


def hexdigest(algorithm, data):
    if algorithm == "sha256":
        return hashlib.sha256(data).hexdigest()
    return blake3.blake3(data).hexdigest()


def digest_fields(domain, fields):
    """BLAKE3 over the domain and length-prefixed optional fields (u64 LE)."""
    h = blake3.blake3()
    d = domain.encode()
    h.update(struct.pack("<Q", len(d)) + d)
    for field in fields:
        if field is None:
            h.update(b"\x00")
        else:
            b = field.encode()
            h.update(b"\x01" + struct.pack("<Q", len(b)) + b)
    return h.hexdigest()


def dumps(value, sort_keys=False):
    # serde_json's compact form: no whitespace, non-ASCII as raw UTF-8.
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=sort_keys)


def key(campaign, cell, event, content_hash):
    k = {"campaign": campaign, "cell": cell, "event": event, "content_hash": content_hash}
    return digest_fields(KEY_DOMAIN, [campaign, cell, event, content_hash]), k


def record(campaign, cell, event, content_hash, action):
    key_id, k = key(campaign, cell, event, content_hash)
    return {"type": "record", "key_id": key_id, "key": k, "action": action}


def run_line(run_dir, manifest_hash, outcome, campaign, cell, rejections, records):
    entry = {
        "run_id": "",
        "run_dir": run_dir,
        "manifest_hash": manifest_hash,
        "outcome": outcome,
        "campaign": campaign,
        "cell": cell,
        "rejections": rejections,
        "warnings": [],
        "records": records,
    }
    # The id covers every other field: the entry serialized with an empty id.
    entry["run_id"] = digest_fields(RUN_DOMAIN, [dumps(entry)])
    return {"type": "run", **entry}


def plan(algorithm, files, manifest_hash):
    run = json.loads(files["RUN.json"])
    result = json.loads(files["RESULT.json"])
    campaign, cell = run["campaign"], run["cell"]
    data = {p: b.encode() for p, b in files.items()}
    records = []
    campaign_rec = record(campaign, None, "campaign_sample", None, {
        "kind": "create_campaign_sample",
        "route": "episcience POST /api/v1/eln/workflow_runs",
        "canonical_name": run.get("canonical_name") or campaign,
        "workflow_id": run.get("workflow_id"),
        "labels": ["experiment-filer"],
    })
    records.append(campaign_rec)
    run_rec = record(campaign, cell, "run_sample", manifest_hash, {
        "kind": "create_run_sample",
        "route": "episcience POST /api/v1/eln/samples",
        "name": f"{campaign}/{cell}",
        "parent_key_id": campaign_rec["key_id"],
        "status": result["status"],
        "started_at": run.get("started_at"),
        "finished_at": result.get("finished_at"),
    })
    records.append(run_rec)
    records.append(record(campaign, cell, "data_pointer", manifest_hash, {
        "kind": "register_data_pointer",
        "route": "none yet (a0bb3631, c8beb101)",
        "root_uri": run.get("data_root_uri"),
        "manifest_file": MANIFEST_FILE[algorithm],
        "manifest_hash": manifest_hash,
        "file_count": len(files),
        "total_bytes": sum(len(b) for b in data.values()),
    }))
    if "host" in run:
        records.append(record(campaign, cell, "host_observation", manifest_hash, {
            "kind": "add_host_observation",
            "route": "episcience POST /api/v1/eln/samples/:id/observations",
            "sample_key_id": run_rec["key_id"],
            "content": dumps(run["host"], sort_keys=True),
        }))
    for gate in result.get("gates", []):
        records.append(record(campaign, cell, "gate/" + gate["name"], manifest_hash, {
            "kind": "submit_gate_claim",
            "route": "kernel POST /api/v1/claims",
            "gate": gate["name"],
            "verdict": gate["verdict"],
            "content": gate["statement"],
            "methodology": "direct_observation",
            "evidence_type": "empirical",
            "source_url": run.get("data_root_uri"),
            "checksum": manifest_hash,
            "step_id": gate.get("step_id") or run.get("step_id"),
        }))
    for path in run.get("small_files", []):
        digest = algorithm + ":" + hexdigest(algorithm, data[path])
        records.append(record(campaign, cell, "blob/" + path, digest, {
            "kind": "upload_small_file",
            "route": "episcience POST /api/v1/eln/blobs",
            "sample_key_id": run_rec["key_id"],
            "path": path,
            "digest": digest,
        }))
    return campaign, cell, records


def main():
    lines = []
    seen = set()
    for run_dir in sorted(RUNS):
        algorithm, listed, on_disk = RUNS[run_dir]
        if algorithm is None:
            continue
        manifest = "".join(
            f"{hexdigest(algorithm, listed[p].encode())}  {p}\n" for p in sorted(listed)
        ).encode()
        manifest_hash = algorithm + ":" + hexdigest(algorithm, manifest)
        if on_disk:
            rejections = [
                {
                    "reason": "manifest_invalid",
                    "manifest": algorithm,
                    "issue": {
                        "issue": "hash_mismatch",
                        "path": p,
                        "expected": hexdigest(algorithm, listed[p].encode()),
                        "actual": hexdigest(algorithm, on_disk[p].encode()),
                    },
                }
                for p in sorted(on_disk)
            ]
            lines.append(run_line(run_dir, manifest_hash, "rejected", None, None, rejections, []))
            continue
        campaign, cell, records = plan(algorithm, listed, manifest_hash)
        for r in records:
            if r["key_id"] not in seen:
                seen.add(r["key_id"])
                lines.append(r)
        lines.append(run_line(run_dir, manifest_hash, "accepted", campaign, cell, [],
                              [r["key_id"] for r in records]))
    sys.stdout.buffer.write("".join(dumps(l) + "\n" for l in lines).encode())


if __name__ == "__main__":
    main()
