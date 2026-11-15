//! Readers for the run contract files `RUN.json` and `RESULT.json`.
//!
//! No schema for these files exists in this repository or in the kernel
//! repository (searched: code, docs, tests, scripts). The structs below are
//! the minimal schema the backlog items a4002bc2, 28dde255, f376c374 and
//! b35c944e imply. Every field is therefore INVENTED and marked so; each
//! doc comment names the sentence it was derived from. Unknown fields are
//! ignored, so a richer real contract parses as long as these names match.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The run description, written when the run starts.
pub const RUN_FILE: &str = "RUN.json";
/// The run result, written when the run ends.
pub const RESULT_FILE: &str = "RESULT.json";

/// A contract file larger than this is refused unread (it is read whole).
pub const MAX_CONTRACT_BYTES: u64 = 16 * 1024 * 1024;

/// `RUN.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunContract {
    /// INVENTED (required). The campaign this run belongs to; first part of
    /// the deterministic key "(campaign, cell, event, content hash)"
    /// (28dde255) and the name of the campaign's `workflow_run` sample.
    pub campaign: String,
    /// INVENTED (required). The run's cell within the campaign; second part
    /// of the deterministic key and the name of the child sample ("one child
    /// sample per run", a4002bc2).
    pub cell: String,
    /// INVENTED (optional). The EpiGraph workflow UUID, as the existing
    /// `POST /api/v1/eln/workflow_runs` request takes it (`workflow_id`).
    /// Kept as text: phase 0 does not interpret it.
    #[serde(default)]
    pub workflow_id: Option<String>,
    /// INVENTED (optional). The workflow's canonical name, as the existing
    /// `workflow_runs` request takes it (`canonical_name`). Absent: the plan
    /// uses `campaign`.
    #[serde(default)]
    pub canonical_name: Option<String>,
    /// INVENTED (optional). The pre-registered workflow step the gate claims
    /// link to ("linked to the pre-registered workflow step", f376c374).
    #[serde(default)]
    pub step_id: Option<String>,
    /// INVENTED (optional). When the run started, verbatim (RFC 3339
    /// expected, not parsed in phase 0).
    #[serde(default)]
    pub started_at: Option<String>,
    /// INVENTED (optional). Where the run's data lives, as a URI ("source_url
    /// = data root URI", f376c374; "data by pointer", a4002bc2).
    #[serde(default)]
    pub data_root_uri: Option<String>,
    /// INVENTED (optional). Host or instrument readiness, free-form
    /// ("hardware, driver and firmware versions", b35c944e).
    #[serde(default)]
    pub host: Option<serde_json::Value>,
    /// INVENTED (optional). The small files to upload as blobs, relative to
    /// the run directory ("the small files RUN.json names (renders, gate
    /// JSON, logs)", b35c944e). Each must be listed in the manifest.
    #[serde(default)]
    pub small_files: Vec<String>,
}

/// `RESULT.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultContract {
    /// INVENTED (required). How the run ended, verbatim (for example
    /// `completed`, `failed`, `host_lost`). Not an enum: "failed cells,
    /// failed gates and lost hosts are filed like successes" (f376c374).
    pub status: String,
    /// INVENTED (optional). When the run ended, verbatim.
    #[serde(default)]
    pub finished_at: Option<String>,
    /// INVENTED (optional). The gate results of this run.
    #[serde(default)]
    pub gates: Vec<GateResult>,
}

/// One gate result in `RESULT.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateResult {
    /// INVENTED (required). The gate's name, unique within the run; part of
    /// the record's event (`gate/<name>`).
    pub name: String,
    /// INVENTED (required). The verdict, verbatim (for example `PASS`,
    /// `FAIL`).
    pub verdict: String,
    /// INVENTED (required). The claim text, filed unchanged ("gate results
    /// as claims filled word for word from RESULT.json", a4002bc2).
    pub statement: String,
    /// INVENTED (optional). The workflow step this gate belongs to;
    /// overrides `RUN.json`'s `step_id`.
    #[serde(default)]
    pub step_id: Option<String>,
}

/// Why a contract file could not be turned into its struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    /// The file could not be read.
    Unreadable(String),
    /// The file was read but is not a valid contract.
    Invalid(String),
}

/// Checks serde cannot express (non-empty identifiers, unique gate names).
pub trait ContractCheck {
    fn check(&self) -> Result<(), String>;
}

fn non_empty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("`{field}` must not be empty"))
    } else {
        Ok(())
    }
}

impl ContractCheck for RunContract {
    fn check(&self) -> Result<(), String> {
        non_empty("campaign", &self.campaign)?;
        non_empty("cell", &self.cell)
    }
}

impl ContractCheck for ResultContract {
    fn check(&self) -> Result<(), String> {
        non_empty("status", &self.status)?;
        let mut names = BTreeSet::new();
        for gate in &self.gates {
            non_empty("gates[].name", &gate.name)?;
            non_empty("gates[].verdict", &gate.verdict)?;
            non_empty("gates[].statement", &gate.statement)?;
            if !names.insert(gate.name.as_str()) {
                return Err(format!("gate `{}` is listed twice", gate.name));
            }
        }
        Ok(())
    }
}

/// Read and check one contract file. The caller has already resolved `path`
/// inside the run directory (see [`crate::validate::validate_run_dir`]).
pub fn read_json<T: DeserializeOwned + ContractCheck>(path: &Path) -> Result<T, ContractError> {
    let len = fs::metadata(path)
        .map_err(|e| ContractError::Unreadable(e.to_string()))?
        .len();
    if len > MAX_CONTRACT_BYTES {
        return Err(ContractError::Invalid(format!(
            "{len} bytes is over the {MAX_CONTRACT_BYTES}-byte limit"
        )));
    }
    let bytes = fs::read(path).map_err(|e| ContractError::Unreadable(e.to_string()))?;
    let value: T =
        serde_json::from_slice(&bytes).map_err(|e| ContractError::Invalid(e.to_string()))?;
    value.check().map_err(ContractError::Invalid)?;
    Ok(value)
}

/// Read `RUN.json` at `path`.
pub fn read_run(path: &Path) -> Result<RunContract, ContractError> {
    read_json(path)
}

/// Read `RESULT.json` at `path`.
pub fn read_result(path: &Path) -> Result<ResultContract, ContractError> {
    read_json(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_contract_ignores_unknown_fields_and_defaults_optionals() {
        let run: RunContract =
            serde_json::from_str(r#"{"campaign":"c1","cell":"a","engine":{"name":"x"},"seed":7}"#)
                .unwrap();
        assert_eq!(run.campaign, "c1");
        assert_eq!(run.cell, "a");
        assert!(run.workflow_id.is_none());
        assert!(run.small_files.is_empty());
        assert!(run.check().is_ok());
    }

    #[test]
    fn run_contract_requires_campaign_and_cell() {
        assert!(serde_json::from_str::<RunContract>(r#"{"campaign":"c1"}"#).is_err());
        let blank: RunContract = serde_json::from_str(r#"{"campaign":" ","cell":"a"}"#).unwrap();
        assert!(blank.check().is_err());
    }

    #[test]
    fn result_contract_refuses_a_gate_listed_twice() {
        let result: ResultContract = serde_json::from_str(
            r#"{"status":"completed","gates":[
                {"name":"g","verdict":"PASS","statement":"s1"},
                {"name":"g","verdict":"FAIL","statement":"s2"}]}"#,
        )
        .unwrap();
        assert!(result.check().unwrap_err().contains("listed twice"));
    }
}
