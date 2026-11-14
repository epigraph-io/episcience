//! The run-directory validator.
//!
//! A run directory is accepted when it holds a manifest (`MANIFEST.sha256`
//! or `MANIFEST.b3`), `RUN.json` and `RESULT.json`, both JSON files parse
//! into their contract, every manifest present verifies, and (by default)
//! every file in the directory is listed. Anything else is a rejection with
//! one typed [`Rejection`] per failure; all failures are collected, not just
//! the first.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::contract::{
    self, ContractCheck, ContractError, ResultContract, RunContract, RESULT_FILE, RUN_FILE,
};
use crate::errors::FilerError;
use crate::manifest::{
    normalize_rel_path, resolve_inside, verify_manifest_bytes, HashAlgorithm, ManifestIssue,
    ManifestReport, Resolved,
};

/// One of the three contract files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractFile {
    #[serde(rename = "RUN.json")]
    RunJson,
    #[serde(rename = "RESULT.json")]
    ResultJson,
    /// `MANIFEST.sha256` or `MANIFEST.b3` (either satisfies the contract).
    #[serde(rename = "MANIFEST")]
    Manifest,
}

/// One reason a run directory is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Rejection {
    /// A contract file is absent.
    MissingContractFile { file: ContractFile },
    /// A contract file exists but could not be read (or is not a file).
    UnreadableContractFile { file: ContractFile, detail: String },
    /// A contract file was read but is not a valid contract.
    InvalidContractFile { file: ContractFile, detail: String },
    /// A contract file is a symlink that leads out of the run directory.
    ContractFileEscapes { file: ContractFile },
    /// A manifest does not verify.
    ManifestInvalid {
        manifest: HashAlgorithm,
        issue: ManifestIssue,
    },
    /// A file is present in the run directory but no manifest lists it.
    UnlistedFile { path: String },
    /// `RUN.json` names a small file the primary manifest does not list.
    NamedFileNotListed { path: String },
}

/// Validator settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValidatorOptions {
    /// `false` (default): a file no manifest lists rejects the run.
    /// `true`: it is recorded as a warning and the run is accepted.
    pub allow_unlisted: bool,
}

/// A run directory that passed validation.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidRun {
    pub run: RunContract,
    pub result: ResultContract,
    /// The primary manifest's report (see [`HashAlgorithm::ALL`]).
    pub manifest: ManifestReport,
    /// Non-fatal findings (unlisted files under `allow_unlisted`).
    pub warnings: Vec<String>,
}

/// The validator's verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Validation {
    Accepted(Box<ValidRun>),
    Rejected(Vec<Rejection>),
}

fn read_contract<T: DeserializeOwned + ContractCheck>(
    root: &Path,
    file: ContractFile,
    name: &str,
    rejections: &mut Vec<Rejection>,
) -> Option<T> {
    let path = match resolve_inside(root, name) {
        Resolved::File(path) => path,
        Resolved::Missing => {
            rejections.push(Rejection::MissingContractFile { file });
            return None;
        }
        Resolved::Escapes => {
            rejections.push(Rejection::ContractFileEscapes { file });
            return None;
        }
        Resolved::NotAFile => {
            rejections.push(Rejection::UnreadableContractFile {
                file,
                detail: "not a regular file".into(),
            });
            return None;
        }
        Resolved::Io(detail) => {
            rejections.push(Rejection::UnreadableContractFile { file, detail });
            return None;
        }
    };
    match contract::read_json::<T>(&path) {
        Ok(value) => Some(value),
        Err(ContractError::Unreadable(detail)) => {
            rejections.push(Rejection::UnreadableContractFile { file, detail });
            None
        }
        Err(ContractError::Invalid(detail)) => {
            rejections.push(Rejection::InvalidContractFile { file, detail });
            None
        }
    }
}

/// Validate one run directory. Nothing is written, in the directory or
/// anywhere else.
///
/// `Err` only when the directory itself cannot be resolved; every problem
/// with its contents is a [`Rejection`].
pub fn validate_run_dir(
    run_dir: &Path,
    options: &ValidatorOptions,
) -> Result<Validation, FilerError> {
    let root = fs::canonicalize(run_dir).map_err(|e| FilerError::io(run_dir, e))?;
    let mut rejections: Vec<Rejection> = Vec::new();

    let run: Option<RunContract> =
        read_contract(&root, ContractFile::RunJson, RUN_FILE, &mut rejections);
    let result: Option<ResultContract> = read_contract(
        &root,
        ContractFile::ResultJson,
        RESULT_FILE,
        &mut rejections,
    );

    let mut reports: Vec<ManifestReport> = Vec::new();
    let mut manifest_seen = false;
    for algorithm in HashAlgorithm::ALL {
        let file = ContractFile::Manifest;
        match resolve_inside(&root, algorithm.manifest_file()) {
            Resolved::Missing => {}
            Resolved::File(path) => {
                manifest_seen = true;
                match fs::read(&path) {
                    Ok(bytes) => reports.push(verify_manifest_bytes(&root, algorithm, &bytes)),
                    Err(e) => rejections.push(Rejection::UnreadableContractFile {
                        file,
                        detail: e.to_string(),
                    }),
                }
            }
            Resolved::Escapes => {
                manifest_seen = true;
                rejections.push(Rejection::ContractFileEscapes { file });
            }
            Resolved::NotAFile => {
                manifest_seen = true;
                rejections.push(Rejection::UnreadableContractFile {
                    file,
                    detail: "not a regular file".into(),
                });
            }
            Resolved::Io(detail) => {
                manifest_seen = true;
                rejections.push(Rejection::UnreadableContractFile { file, detail });
            }
        }
    }
    if !manifest_seen {
        rejections.push(Rejection::MissingContractFile {
            file: ContractFile::Manifest,
        });
    }

    // A file is unlisted when ANY manifest present leaves it out: each
    // manifest must commit to the whole directory on its own.
    let mut unlisted: BTreeSet<String> = BTreeSet::new();
    for report in &reports {
        for issue in &report.issues {
            rejections.push(Rejection::ManifestInvalid {
                manifest: report.algorithm,
                issue: issue.clone(),
            });
        }
        unlisted.extend(report.unlisted.iter().cloned());
    }
    let mut warnings = Vec::new();
    for path in unlisted {
        if options.allow_unlisted {
            warnings.push(format!("unlisted file: {path}"));
        } else {
            rejections.push(Rejection::UnlistedFile { path });
        }
    }

    let primary = reports.into_iter().next();
    if let (Some(run), Some(primary)) = (&run, &primary) {
        for named in &run.small_files {
            match normalize_rel_path(named) {
                Ok(path) if primary.entries.contains_key(&path) => {}
                Ok(path) => rejections.push(Rejection::NamedFileNotListed { path }),
                Err(_) => rejections.push(Rejection::InvalidContractFile {
                    file: ContractFile::RunJson,
                    detail: format!("small_files entry `{named}` is not a path inside the run"),
                }),
            }
        }
    }

    match (run, result, primary) {
        (Some(run), Some(result), Some(manifest)) if rejections.is_empty() => {
            Ok(Validation::Accepted(Box::new(ValidRun {
                run,
                result,
                manifest,
                warnings,
            })))
        }
        _ => Ok(Validation::Rejected(rejections)),
    }
}
