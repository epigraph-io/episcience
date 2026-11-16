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
    self, ContractCheck, ContractError, ResultContract, RunContract, MAX_MANIFEST_BYTES,
    RESULT_FILE, RUN_FILE,
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
    /// The `RUN.json` or `RESULT.json` bytes that were parsed are not the
    /// bytes `manifest` lists for it, although the file on disk matched when
    /// the manifest was verified: it changed while the run directory was being
    /// validated. Only attested bytes are ever filed.
    ContractFileChanged {
        file: ContractFile,
        manifest: HashAlgorithm,
    },
    /// A directory below the watched root that the watcher cannot scan: its
    /// name is not UTF-8, or listing it failed. Its subtree is skipped, and
    /// the rejection is keyed by its path alone (see [`crate::watcher`]).
    UnscannableDirectory { detail: String },
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

/// Read, parse and check one JSON contract file. Returns the value and the
/// exact bytes it was parsed from.
fn read_contract<T: DeserializeOwned + ContractCheck>(
    root: &Path,
    file: ContractFile,
    name: &str,
    rejections: &mut Vec<Rejection>,
) -> Option<(T, Vec<u8>)> {
    // The resolution sorts out a symlink leading OUT of the run directory
    // (`ContractFileEscapes`); the read below then refuses any symlink, and
    // anything but a regular file, under the contract file's own name.
    match resolve_inside(root, name) {
        Resolved::File(_) => {}
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
    }
    let parsed = contract::read_contract_bytes(&root.join(name), contract::MAX_CONTRACT_BYTES)
        .and_then(|bytes| contract::parse_json::<T>(&bytes).map(|value| (value, bytes)));
    match parsed {
        Ok(parsed) => Some(parsed),
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

    let run: Option<(RunContract, Vec<u8>)> =
        read_contract(&root, ContractFile::RunJson, RUN_FILE, &mut rejections);
    let result: Option<(ResultContract, Vec<u8>)> = read_contract(
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
            Resolved::File(_) => {
                manifest_seen = true;
                let path = root.join(algorithm.manifest_file());
                match contract::read_contract_bytes(&path, MAX_MANIFEST_BYTES) {
                    Ok(bytes) => reports.push(verify_manifest_bytes(&root, algorithm, &bytes)),
                    Err(ContractError::Unreadable(detail)) => {
                        rejections.push(Rejection::UnreadableContractFile { file, detail });
                    }
                    Err(ContractError::Invalid(detail)) => {
                        rejections.push(Rejection::InvalidContractFile { file, detail });
                    }
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
    rejections.extend(unattested(
        &reports,
        run.as_ref().map(|r| r.1.as_slice()),
        result.as_ref().map(|r| r.1.as_slice()),
    ));

    let mut warnings = Vec::new();
    for path in unlisted {
        if options.allow_unlisted {
            warnings.push(format!("unlisted file: {path}"));
        } else {
            rejections.push(Rejection::UnlistedFile { path });
        }
    }

    let run = run.map(|(run, _)| run);
    let result = result.map(|(result, _)| result);
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

/// The path a manifest issue is about, if it names one.
fn issue_path(issue: &ManifestIssue) -> Option<&str> {
    match issue {
        ManifestIssue::PathTraversal { path, .. }
        | ManifestIssue::DuplicateEntry { path, .. }
        | ManifestIssue::MissingFile { path }
        | ManifestIssue::NotARegularFile { path }
        | ManifestIssue::HashMismatch { path, .. }
        | ManifestIssue::Io { path, .. } => Some(path),
        ManifestIssue::MalformedLine { .. } | ManifestIssue::Empty => None,
    }
}

/// Whether the parsed `bytes` of contract file `name` are what `report`'s
/// manifest lists for it. `true` when the manifest does not list `name` (an
/// unlisted file is its own rejection, or a warning under `allow_unlisted`)
/// and when the manifest already reports an issue on `name` (that issue is
/// the rejection; this check adds the case where the file on disk verified
/// but the bytes that were parsed differ).
fn parsed_bytes_attested(report: &ManifestReport, name: &str, bytes: &[u8]) -> bool {
    if report.issues.iter().any(|i| issue_path(i) == Some(name)) {
        return true;
    }
    match report.entries.get(name) {
        None => true,
        Some(listed) => report.algorithm.hash_bytes(bytes) == *listed,
    }
}

/// The JSON files were read (and parsed) before the manifests re-read them
/// from disk to hash them. File only bytes every present manifest attests: a
/// file swapped in between must not be filed under a hash that does not cover
/// what was parsed. One [`Rejection::ContractFileChanged`] per contract file
/// and manifest that disagree.
fn unattested(
    reports: &[ManifestReport],
    run_bytes: Option<&[u8]>,
    result_bytes: Option<&[u8]>,
) -> Vec<Rejection> {
    let mut rejections = Vec::new();
    for report in reports {
        for (file, name, parsed) in [
            (ContractFile::RunJson, RUN_FILE, run_bytes),
            (ContractFile::ResultJson, RESULT_FILE, result_bytes),
        ] {
            if let Some(bytes) = parsed {
                if !parsed_bytes_attested(report, name, bytes) {
                    rejections.push(Rejection::ContractFileChanged {
                        file,
                        manifest: report.algorithm,
                    });
                }
            }
        }
    }
    rejections
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn report(algorithm: HashAlgorithm, listed: &[(&str, &[u8])]) -> ManifestReport {
        ManifestReport {
            algorithm,
            manifest_hash: algorithm.tagged("00"),
            entries: listed
                .iter()
                .map(|(name, bytes)| ((*name).to_string(), algorithm.hash_bytes(bytes)))
                .collect::<BTreeMap<_, _>>(),
            verified: listed.len(),
            total_bytes: 0,
            issues: Vec::new(),
            unlisted: Vec::new(),
        }
    }

    #[test]
    fn parsed_bytes_must_match_every_present_manifest() {
        let (run, result) = (br#"{"a":1}"#.as_slice(), b"{}".as_slice());
        for algorithm in HashAlgorithm::ALL {
            let both = report(algorithm, &[(RUN_FILE, run), (RESULT_FILE, result)]);
            assert!(parsed_bytes_attested(&both, RUN_FILE, run));
            assert!(parsed_bytes_attested(&both, RESULT_FILE, result));
            // Bytes that differ from what the manifest lists.
            assert!(!parsed_bytes_attested(&both, RUN_FILE, br#"{"a":2}"#));
            assert!(!parsed_bytes_attested(&both, RESULT_FILE, b"[]"));
        }

        // A second manifest that attests other bytes is checked on its own.
        let sha = report(HashAlgorithm::Sha256, &[(RUN_FILE, run)]);
        let b3 = report(HashAlgorithm::Blake3, &[(RUN_FILE, b"{}")]);
        assert!(parsed_bytes_attested(&sha, RUN_FILE, run));
        assert!(!parsed_bytes_attested(&b3, RUN_FILE, run));

        // Not listed: the unlisted-file rule decides, not this one.
        assert!(parsed_bytes_attested(&sha, RESULT_FILE, b"anything"));

        // Already reported on: no second rejection for the same file.
        let mut mismatched = report(HashAlgorithm::Sha256, &[(RUN_FILE, b"{}")]);
        mismatched.issues.push(ManifestIssue::HashMismatch {
            path: RUN_FILE.into(),
            expected: "e".into(),
            actual: "a".into(),
        });
        assert!(parsed_bytes_attested(&mismatched, RUN_FILE, run));
    }

    #[test]
    fn each_disagreeing_file_and_manifest_is_one_rejection() {
        let (run, result) = (br#"{"a":1}"#.as_slice(), b"{}".as_slice());
        let reports = [
            report(
                HashAlgorithm::Sha256,
                &[(RUN_FILE, run), (RESULT_FILE, result)],
            ),
            report(
                HashAlgorithm::Blake3,
                &[(RUN_FILE, run), (RESULT_FILE, b"[]")],
            ),
        ];
        assert_eq!(unattested(&reports[..1], Some(run), Some(result)), vec![]);
        assert_eq!(
            unattested(&reports, Some(run), Some(result)),
            vec![Rejection::ContractFileChanged {
                file: ContractFile::ResultJson,
                manifest: HashAlgorithm::Blake3
            }]
        );
        assert_eq!(
            unattested(&reports, Some(b"{}"), None),
            vec![
                Rejection::ContractFileChanged {
                    file: ContractFile::RunJson,
                    manifest: HashAlgorithm::Sha256
                },
                Rejection::ContractFileChanged {
                    file: ContractFile::RunJson,
                    manifest: HashAlgorithm::Blake3
                },
            ]
        );
    }
}
