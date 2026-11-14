//! The shadow filing plan: what a later phase WOULD write for one accepted
//! run, one record per write, each under a deterministic key.
//!
//! Nothing here performs a write. `route` fields name the existing surface a
//! later phase calls (see `docs/experiment-filer.md` for the inventory); they
//! are text in a journal line, not a client.
//!
//! The plan is a pure function of the contract files: no clock, no random
//! id, no absolute path. Replaying the same run directories yields the same
//! bytes.

use serde::{Deserialize, Serialize};

use crate::manifest::normalize_rel_path;
use crate::validate::ValidRun;

/// `crates/episcience-api/src/routes/workflow_runs.rs`.
pub const ROUTE_WORKFLOW_RUNS: &str = "episcience POST /api/v1/eln/workflow_runs";
/// `crates/episcience-api/src/routes/samples.rs` (`create_sample`).
pub const ROUTE_SAMPLES: &str = "episcience POST /api/v1/eln/samples";
/// `crates/episcience-api/src/routes/samples.rs` (`add_observation`).
pub const ROUTE_OBSERVATIONS: &str = "episcience POST /api/v1/eln/samples/:id/observations";
/// `crates/episcience-api/src/routes/blobs.rs` (`upload_blob`, multipart).
pub const ROUTE_BLOBS: &str = "episcience POST /api/v1/eln/blobs";
/// The kernel's claim route (`epigraph-api`, `routes/claims.rs`).
pub const ROUTE_KERNEL_CLAIMS: &str = "kernel POST /api/v1/claims";
/// No route yet: pointer-only blobs are backlog a0bb3631, dataset evidence
/// is backlog c8beb101.
pub const ROUTE_DATA_POINTER: &str = "none yet (a0bb3631, c8beb101)";

const KEY_DOMAIN: &str = "episcience-filer/v1/key";

/// BLAKE3 over a domain string and length-prefixed optional fields, so no
/// two different field lists share an encoding.
pub(crate) fn digest_fields(domain: &str, fields: &[Option<&str>]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(domain.len() as u64).to_le_bytes());
    hasher.update(domain.as_bytes());
    for field in fields {
        match field {
            None => {
                hasher.update(&[0u8]);
            }
            Some(value) => {
                hasher.update(&[1u8]);
                hasher.update(&(value.len() as u64).to_le_bytes());
                hasher.update(value.as_bytes());
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// The deterministic key of one record: "(campaign, cell, event, content
/// hash)" (28dde255).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeterministicKey {
    pub campaign: String,
    /// `None` for a campaign-level record.
    pub cell: Option<String>,
    /// What is filed: `campaign_sample`, `run_sample`, `data_pointer`,
    /// `host_observation`, `gate/<name>`, `blob/<path>`.
    pub event: String,
    /// `<algorithm>:<hex>`: the manifest hash for run-level records, the
    /// file's own digest for a blob, `None` for the campaign sample (it must
    /// not change when a run is added).
    pub content_hash: Option<String>,
}

impl DeterministicKey {
    /// One fixed-length id for the key (BLAKE3, hex): what the journal
    /// de-duplicates on and what a writer looks up before writing.
    #[must_use]
    pub fn id(&self) -> String {
        digest_fields(
            KEY_DOMAIN,
            &[
                Some(self.campaign.as_str()),
                self.cell.as_deref(),
                Some(self.event.as_str()),
                self.content_hash.as_deref(),
            ],
        )
    }
}

/// What a later phase would write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlannedAction {
    /// The campaign as a `workflow_run` sample.
    CreateCampaignSample {
        route: String,
        canonical_name: String,
        workflow_id: Option<String>,
        labels: Vec<String>,
    },
    /// The run as a child sample of the campaign sample.
    CreateRunSample {
        route: String,
        name: String,
        /// `key_id` of the campaign sample record.
        parent_key_id: String,
        status: String,
        started_at: Option<String>,
        finished_at: Option<String>,
    },
    /// The run's data, by pointer and manifest hash; no bytes move.
    RegisterDataPointer {
        route: String,
        root_uri: Option<String>,
        manifest_file: String,
        manifest_hash: String,
        file_count: usize,
        total_bytes: u64,
    },
    /// Host or instrument readiness as an observation on the run sample.
    AddHostObservation {
        route: String,
        /// `key_id` of the run sample record.
        sample_key_id: String,
        /// `RUN.json`'s `host` value as compact JSON.
        content: String,
    },
    /// One gate result as a kernel claim.
    SubmitGateClaim {
        route: String,
        gate: String,
        verdict: String,
        /// `RESULT.json`'s `statement`, unchanged.
        content: String,
        methodology: String,
        evidence_type: String,
        source_url: Option<String>,
        /// The manifest hash, `<algorithm>:<hex>`.
        checksum: String,
        step_id: Option<String>,
    },
    /// One small file `RUN.json` names, as a blob on the run sample.
    UploadSmallFile {
        route: String,
        /// `key_id` of the run sample record.
        sample_key_id: String,
        path: String,
        digest: String,
    },
}

/// One line of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRecord {
    pub key_id: String,
    pub key: DeterministicKey,
    pub action: PlannedAction,
}

impl PlanRecord {
    fn new(key: DeterministicKey, action: PlannedAction) -> Self {
        Self {
            key_id: key.id(),
            key,
            action,
        }
    }
}

/// The plan for one accepted run, in a fixed order: campaign sample, run
/// sample, data pointer, host observation, gates (file order), small files
/// (file order).
///
/// Not planned in phase 0: the workflow outcome at campaign close. No
/// contract file says a campaign has closed; that comes from the
/// orchestrator's log, whose format is not defined anywhere yet.
#[must_use]
pub fn build_plan(valid: &ValidRun) -> Vec<PlanRecord> {
    let run = &valid.run;
    let result = &valid.result;
    let manifest = &valid.manifest;
    let algorithm = manifest.algorithm;
    let run_key = |event: String| DeterministicKey {
        campaign: run.campaign.clone(),
        cell: Some(run.cell.clone()),
        event,
        content_hash: Some(manifest.manifest_hash.clone()),
    };

    let mut records = Vec::new();

    let campaign = PlanRecord::new(
        DeterministicKey {
            campaign: run.campaign.clone(),
            cell: None,
            event: "campaign_sample".into(),
            content_hash: None,
        },
        PlannedAction::CreateCampaignSample {
            route: ROUTE_WORKFLOW_RUNS.into(),
            canonical_name: run
                .canonical_name
                .clone()
                .unwrap_or_else(|| run.campaign.clone()),
            workflow_id: run.workflow_id.clone(),
            labels: vec!["experiment-filer".into()],
        },
    );
    let campaign_key_id = campaign.key_id.clone();
    records.push(campaign);

    let run_sample = PlanRecord::new(
        run_key("run_sample".into()),
        PlannedAction::CreateRunSample {
            route: ROUTE_SAMPLES.into(),
            name: format!("{}/{}", run.campaign, run.cell),
            parent_key_id: campaign_key_id,
            status: result.status.clone(),
            started_at: run.started_at.clone(),
            finished_at: result.finished_at.clone(),
        },
    );
    let run_sample_key_id = run_sample.key_id.clone();
    records.push(run_sample);

    records.push(PlanRecord::new(
        run_key("data_pointer".into()),
        PlannedAction::RegisterDataPointer {
            route: ROUTE_DATA_POINTER.into(),
            root_uri: run.data_root_uri.clone(),
            manifest_file: algorithm.manifest_file().into(),
            manifest_hash: manifest.manifest_hash.clone(),
            file_count: manifest.entries.len(),
            total_bytes: manifest.total_bytes,
        },
    ));

    if let Some(host) = &run.host {
        records.push(PlanRecord::new(
            run_key("host_observation".into()),
            PlannedAction::AddHostObservation {
                route: ROUTE_OBSERVATIONS.into(),
                sample_key_id: run_sample_key_id.clone(),
                content: host.to_string(),
            },
        ));
    }

    for gate in &result.gates {
        records.push(PlanRecord::new(
            run_key(format!("gate/{}", gate.name)),
            PlannedAction::SubmitGateClaim {
                route: ROUTE_KERNEL_CLAIMS.into(),
                gate: gate.name.clone(),
                verdict: gate.verdict.clone(),
                content: gate.statement.clone(),
                methodology: "direct_observation".into(),
                evidence_type: "empirical".into(),
                source_url: run.data_root_uri.clone(),
                checksum: manifest.manifest_hash.clone(),
                step_id: gate.step_id.clone().or_else(|| run.step_id.clone()),
            },
        ));
    }

    for named in &run.small_files {
        // The validator has already refused a name that does not normalize
        // or is not in the manifest; skipping here keeps this total.
        let Ok(path) = normalize_rel_path(named) else {
            continue;
        };
        let Some(digest) = manifest.entries.get(&path) else {
            continue;
        };
        let digest = algorithm.tagged(digest);
        records.push(PlanRecord::new(
            DeterministicKey {
                campaign: run.campaign.clone(),
                cell: Some(run.cell.clone()),
                event: format!("blob/{path}"),
                content_hash: Some(digest.clone()),
            },
            PlannedAction::UploadSmallFile {
                route: ROUTE_BLOBS.into(),
                sample_key_id: run_sample_key_id.clone(),
                path,
                digest,
            },
        ));
    }

    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_id_separates_fields_and_absent_from_empty() {
        let key = |campaign: &str, cell: Option<&str>, event: &str| DeterministicKey {
            campaign: campaign.into(),
            cell: cell.map(str::to_string),
            event: event.into(),
            content_hash: None,
        };
        // Same concatenation, different split.
        assert_ne!(
            key("ab", Some("c"), "e").id(),
            key("a", Some("bc"), "e").id()
        );
        // Absent is not empty.
        assert_ne!(key("a", None, "e").id(), key("a", Some(""), "e").id());
        // Stable.
        assert_eq!(key("a", Some("b"), "e").id(), key("a", Some("b"), "e").id());
        assert_eq!(key("a", Some("b"), "e").id().len(), 64);
    }
}
