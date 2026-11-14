//! The shadow filing plan: deterministic keys and the records they name.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::canonical::{content_hash, to_canonical_string};
use crate::contract::{ContractError, RunContract};

/// A record's deterministic key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    pub campaign: String,
    pub cell: String,
    pub event: String,
    pub content_hash: String,
}

impl Key {
    fn to_value(&self) -> Value {
        json!({
            "campaign": self.campaign,
            "cell": self.cell,
            "event": self.event,
            "content_hash": self.content_hash,
        })
    }

    /// `sha256:<hex>` of the key's canonical JSON: the id a writer looks up
    /// before it files the record.
    pub fn id(&self) -> String {
        content_hash(&self.to_value())
    }
}

/// One line of the filing plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRecord {
    pub key: Key,
    pub kind: String,
    pub payload: Value,
}

/// A plan line that does not decode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("plan line: {0}")]
pub struct DecodeError(pub String);

impl PlanRecord {
    /// `sha256:<hex>` of the canonical {key, kind, payload}: an integrity
    /// check over the whole record (it detects an edit or a bit flip; it is not
    /// a signature).
    pub fn record_hash(&self) -> String {
        content_hash(&json!({
            "key": self.key.to_value(),
            "kind": self.kind,
            "payload": self.payload,
        }))
    }

    pub fn to_value(&self) -> Value {
        json!({
            "key_id": self.key.id(),
            "key": self.key.to_value(),
            "kind": self.kind,
            "payload": self.payload,
            "record_hash": self.record_hash(),
        })
    }

    /// The record as one canonical JSON line, without the newline.
    pub fn to_line(&self) -> String {
        to_canonical_string(&self.to_value())
    }

    /// Decode a plan line. It must be exactly the canonical line its fields
    /// render to, so an edited `key_id`, key, kind or payload (which no longer
    /// matches `key_id` or `record_hash`) does not decode.
    pub fn from_line(line: &str) -> Result<PlanRecord, DecodeError> {
        let v: Value = serde_json::from_str(line).map_err(|e| DecodeError(e.to_string()))?;
        let field = |obj: &Value, name: &str| -> Result<String, DecodeError> {
            obj.get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| DecodeError(format!("`{name}` must be a string")))
        };
        let key_v = v
            .get("key")
            .ok_or_else(|| DecodeError("`key` is missing".to_string()))?;
        let record = PlanRecord {
            key: Key {
                campaign: field(key_v, "campaign")?,
                cell: field(key_v, "cell")?,
                event: field(key_v, "event")?,
                content_hash: field(key_v, "content_hash")?,
            },
            kind: field(&v, "kind")?,
            payload: v.get("payload").cloned().unwrap_or(Value::Null),
        };
        if record.to_line() != line {
            return Err(DecodeError(
                "is not the canonical rendering of its own fields (edited or foreign)".to_string(),
            ));
        }
        Ok(record)
    }
}

/// What the scanner found in one run directory.
#[derive(Debug, Clone, PartialEq)]
pub enum ScanOutcome {
    Valid {
        run_dir: String,
        contract: RunContract,
    },
    /// A rejected directory. It carries no campaign or cell: those would
    /// come from bytes no manifest attests.
    Rejected {
        run_dir: String,
        reason: ContractError,
    },
}

fn campaign_record(campaign: &str) -> PlanRecord {
    let payload = json!({ "campaign": campaign });
    PlanRecord {
        key: Key {
            campaign: campaign.to_string(),
            cell: String::new(),
            event: "campaign".to_string(),
            content_hash: content_hash(&payload),
        },
        kind: "campaign".to_string(),
        payload,
    }
}

/// The records one scanned directory contributes (its campaign record aside).
pub fn records_for(outcome: &ScanOutcome) -> Vec<PlanRecord> {
    match outcome {
        ScanOutcome::Valid { run_dir, contract } => {
            let manifest_hash = contract.primary_manifest_hash().to_string();
            let manifests: Vec<Value> = contract
                .manifests
                .iter()
                .map(|m| {
                    json!({
                        "file": m.algorithm.manifest_file(),
                        "algorithm": m.algorithm.name(),
                        "hash": m.manifest_hash,
                        "entries": m.entries,
                    })
                })
                .collect();
            let mut out = vec![PlanRecord {
                key: Key {
                    campaign: contract.campaign.clone(),
                    cell: contract.cell.clone(),
                    event: "run".to_string(),
                    content_hash: manifest_hash.clone(),
                },
                kind: "run".to_string(),
                payload: json!({
                    "run_dir": run_dir,
                    "manifests": manifests,
                    "run": contract.run,
                    "result": contract.result,
                }),
            }];
            for gate in &contract.gates {
                // The hash covers the gate AND its run, so the same verdict
                // from a re-run of the cell is a new record, while re-reading
                // the same run gives the same key.
                let hashed = json!({ "gate": gate.body, "run_manifest_hash": manifest_hash });
                let payload = json!({
                    "gate": gate.body,
                    "passed": gate.passed,
                    "run_manifest_hash": manifest_hash,
                    "run_dir": run_dir,
                });
                out.push(PlanRecord {
                    key: Key {
                        campaign: contract.campaign.clone(),
                        cell: contract.cell.clone(),
                        event: format!("gate:{}", gate.name),
                        content_hash: content_hash(&hashed),
                    },
                    kind: "gate".to_string(),
                    payload,
                });
            }
            out
        }
        ScanOutcome::Rejected { run_dir, reason } => {
            // Keyed by the directory and reason only, with an empty campaign
            // and cell: an unverified RUN.json must not be able to file a
            // rejection under another run's campaign or cell.
            let payload = json!({ "run_dir": run_dir, "reason": reason.to_string() });
            vec![PlanRecord {
                key: Key {
                    campaign: String::new(),
                    cell: String::new(),
                    event: "rejected".to_string(),
                    content_hash: content_hash(&payload),
                },
                kind: "rejection".to_string(),
                payload,
            }]
        }
    }
}

/// Sort records by key (then line, so ties are total) and keep the first
/// record of each `key_id`.
pub fn normalize(mut records: Vec<PlanRecord>) -> Vec<PlanRecord> {
    let mut keyed: Vec<(Key, String, PlanRecord)> = records
        .drain(..)
        .map(|r| (r.key.clone(), r.to_line(), r))
        .collect();
    keyed.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut seen = BTreeSet::new();
    keyed
        .into_iter()
        .filter(|(key, _, _)| seen.insert(key.id()))
        .map(|(_, _, r)| r)
        .collect()
}

/// The full plan for a set of scan outcomes, in any order.
pub fn build_plan(outcomes: &[ScanOutcome]) -> Vec<PlanRecord> {
    let mut records = Vec::new();
    let mut campaigns = BTreeSet::new();
    for outcome in outcomes {
        if let ScanOutcome::Valid { contract, .. } = outcome {
            campaigns.insert(contract.campaign.clone());
        }
        records.extend(records_for(outcome));
    }
    records.extend(campaigns.iter().map(|c| campaign_record(c)));
    normalize(records)
}

/// Render a plan: one canonical line per record, each ending in `\n`.
pub fn render(records: &[PlanRecord]) -> String {
    let mut out = String::new();
    for r in records {
        out.push_str(&r.to_line());
        out.push('\n');
    }
    out
}
