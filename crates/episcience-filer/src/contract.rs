//! Reader and verifier for a run directory's contract files.
//! The rules are in the crate docs.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

pub const RUN_FILE: &str = "RUN.json";
pub const RESULT_FILE: &str = "RESULT.json";
pub const MANIFEST_SHA256: &str = "MANIFEST.sha256";
pub const MANIFEST_B3: &str = "MANIFEST.b3";

/// Every file name that marks a directory as a run directory.
pub const CONTRACT_FILES: [&str; 4] = [RUN_FILE, RESULT_FILE, MANIFEST_SHA256, MANIFEST_B3];

/// A manifest's digest algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Sha256,
    Blake3,
}

impl Algorithm {
    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Sha256 => "sha256",
            Algorithm::Blake3 => "blake3",
        }
    }

    pub fn manifest_file(self) -> &'static str {
        match self {
            Algorithm::Sha256 => MANIFEST_SHA256,
            Algorithm::Blake3 => MANIFEST_B3,
        }
    }

    /// Lowercase hex digest of `bytes`.
    pub fn digest_bytes(self, bytes: &[u8]) -> String {
        match self {
            Algorithm::Sha256 => hex::encode(Sha256::digest(bytes)),
            Algorithm::Blake3 => blake3::hash(bytes).to_hex().to_string(),
        }
    }

    /// Lowercase hex digest of the file at `path`, streamed.
    pub fn digest_file(self, path: &Path) -> io::Result<String> {
        let mut reader = BufReader::with_capacity(1 << 16, File::open(path)?);
        match self {
            Algorithm::Sha256 => {
                let mut h = Sha256::new();
                io::copy(&mut reader, &mut h)?;
                Ok(hex::encode(h.finalize()))
            }
            Algorithm::Blake3 => {
                let mut h = blake3::Hasher::new();
                io::copy(&mut reader, &mut h)?;
                Ok(h.finalize().to_hex().to_string())
            }
        }
    }
}

/// Why a run directory was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContractError {
    #[error("missing {0}")]
    MissingFile(&'static str),
    #[error("missing manifest: neither MANIFEST.sha256 nor MANIFEST.b3 is present")]
    MissingManifest,
    #[error("{file} is empty: it lists no file")]
    EmptyManifest { file: &'static str },
    #[error("{file} line {line}: {reason}")]
    ManifestSyntax {
        file: &'static str,
        line: usize,
        reason: String,
    },
    #[error("{file} line {line}: path {path:?} {reason}")]
    ManifestPath {
        file: &'static str,
        line: usize,
        path: String,
        reason: &'static str,
    },
    #[error("{file} does not list {path}")]
    NotListed {
        file: &'static str,
        path: &'static str,
    },
    #[error("{file} lists {path}, which is missing or unreadable: {reason}")]
    ListedFileUnreadable {
        file: &'static str,
        path: String,
        reason: String,
    },
    #[error("{file}: digest mismatch for {path}: manifest {expected}, file {actual}")]
    DigestMismatch {
        file: &'static str,
        path: String,
        expected: String,
        actual: String,
    },
    #[error("{file} is not valid JSON: {reason}")]
    InvalidJson { file: &'static str, reason: String },
    #[error("{file}: {reason}")]
    Schema { file: &'static str, reason: String },
    #[error("cannot read {file}: {reason}")]
    Unreadable { file: &'static str, reason: String },
}

/// One line of a manifest, after path normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub digest: String,
    pub path: String,
}

/// A manifest whose every entry verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    pub algorithm: Algorithm,
    /// `<algorithm>:<hex>` of the manifest file's own bytes.
    pub manifest_hash: String,
    pub entries: usize,
}

/// A gate from `RESULT.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Gate {
    pub name: String,
    pub passed: bool,
    /// The gate object exactly as parsed.
    pub body: Value,
}

/// A run directory that passed every contract rule.
#[derive(Debug, Clone, PartialEq)]
pub struct RunContract {
    pub campaign: String,
    pub cell: String,
    pub run: Value,
    pub result: Value,
    pub gates: Vec<Gate>,
    /// Verified manifests, `MANIFEST.sha256` first.
    pub manifests: Vec<VerifiedManifest>,
}

impl RunContract {
    /// The manifest hash that identifies this run's content.
    pub fn primary_manifest_hash(&self) -> &str {
        &self.manifests[0].manifest_hash
    }
}

/// Parse a `sha256sum`/`b3sum` manifest.
pub fn parse_manifest(file: &'static str, text: &str) -> Result<Vec<ManifestEntry>, ContractError> {
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    for (idx, raw) in text.split('\n').enumerate() {
        let line = idx + 1;
        if raw.is_empty() {
            continue;
        }
        let syntax = |reason: &str| ContractError::ManifestSyntax {
            file,
            line,
            reason: reason.to_string(),
        };
        if raw.starts_with('\\') {
            return Err(syntax("escaped file names are not supported"));
        }
        if raw.len() < 67 || !raw.is_char_boundary(64) {
            return Err(syntax("expected `<64 hex digits>  <path>`"));
        }
        let (digest, rest) = raw.split_at(64);
        if !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(syntax("digest is not 64 lowercase hex digits"));
        }
        let path = if let Some(p) = rest.strip_prefix("  ") {
            p
        } else if let Some(p) = rest.strip_prefix(" *") {
            p
        } else {
            return Err(syntax("digest and path must be separated by `  ` or ` *`"));
        };
        let path = normalize_path(path).map_err(|reason| ContractError::ManifestPath {
            file,
            line,
            path: path.to_string(),
            reason,
        })?;
        if path == MANIFEST_SHA256 || path == MANIFEST_B3 {
            return Err(ContractError::ManifestPath {
                file,
                line,
                path,
                reason: "is a manifest; a manifest cannot attest a manifest",
            });
        }
        if !seen.insert(path.clone()) {
            return Err(ContractError::ManifestPath {
                file,
                line,
                path,
                reason: "is listed twice",
            });
        }
        entries.push(ManifestEntry {
            digest: digest.to_string(),
            path,
        });
    }
    if entries.is_empty() {
        return Err(ContractError::EmptyManifest { file });
    }
    Ok(entries)
}

/// Normalize a manifest path: `/`-separated, relative, no `..`, `.` dropped.
fn normalize_path(path: &str) -> Result<String, &'static str> {
    if path.is_empty() {
        return Err("is empty");
    }
    if path.starts_with('/') {
        return Err("is absolute");
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err("contains `..`"),
            p => parts.push(p),
        }
    }
    if parts.is_empty() {
        return Err("names no file");
    }
    Ok(parts.join("/"))
}

fn read_contract_file(dir: &Path, file: &'static str) -> Result<Option<Vec<u8>>, ContractError> {
    match std::fs::read(dir.join(file)) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ContractError::Unreadable {
            file,
            reason: e.to_string(),
        }),
    }
}

/// Verify one manifest against the files in `dir`.
fn verify_manifest(
    dir: &Path,
    canonical_dir: &Path,
    algorithm: Algorithm,
    bytes: &[u8],
) -> Result<VerifiedManifest, ContractError> {
    let file = algorithm.manifest_file();
    let text = std::str::from_utf8(bytes).map_err(|_| ContractError::ManifestSyntax {
        file,
        line: 0,
        reason: "is not UTF-8".to_string(),
    })?;
    let entries = parse_manifest(file, text)?;
    for required in [RUN_FILE, RESULT_FILE] {
        if !entries.iter().any(|e| e.path == required) {
            return Err(ContractError::NotListed {
                file,
                path: required,
            });
        }
    }
    for entry in &entries {
        let target: PathBuf = dir.join(&entry.path);
        let unreadable = |reason: String| ContractError::ListedFileUnreadable {
            file,
            path: entry.path.clone(),
            reason,
        };
        let resolved = target
            .canonicalize()
            .map_err(|e| unreadable(e.to_string()))?;
        if !resolved.starts_with(canonical_dir) {
            return Err(unreadable(
                "it resolves outside the run directory".to_string(),
            ));
        }
        if !resolved.is_file() {
            return Err(unreadable("it is not a regular file".to_string()));
        }
        let actual = algorithm
            .digest_file(&resolved)
            .map_err(|e| unreadable(e.to_string()))?;
        if actual != entry.digest {
            return Err(ContractError::DigestMismatch {
                file,
                path: entry.path.clone(),
                expected: entry.digest.clone(),
                actual,
            });
        }
    }
    Ok(VerifiedManifest {
        algorithm,
        manifest_hash: format!("{}:{}", algorithm.name(), algorithm.digest_bytes(bytes)),
        entries: entries.len(),
    })
}

fn parse_json(file: &'static str, bytes: &[u8]) -> Result<Value, ContractError> {
    serde_json::from_slice(bytes).map_err(|e| ContractError::InvalidJson {
        file,
        reason: e.to_string(),
    })
}

fn required_string(file: &'static str, obj: &Value, field: &str) -> Result<String, ContractError> {
    match obj.get(field) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(ContractError::Schema {
            file,
            reason: format!("`{field}` must be a non-empty string"),
        }),
    }
}

/// Read `campaign` and `cell` from `RUN.json` bytes.
pub fn parse_run(bytes: &[u8]) -> Result<(String, String, Value), ContractError> {
    let run = parse_json(RUN_FILE, bytes)?;
    if !run.is_object() {
        return Err(ContractError::Schema {
            file: RUN_FILE,
            reason: "must be a JSON object".to_string(),
        });
    }
    let campaign = required_string(RUN_FILE, &run, "campaign")?;
    let cell = required_string(RUN_FILE, &run, "cell")?;
    Ok((campaign, cell, run))
}

/// Read the gates from `RESULT.json` bytes.
pub fn parse_result(bytes: &[u8]) -> Result<(Vec<Gate>, Value), ContractError> {
    let result = parse_json(RESULT_FILE, bytes)?;
    let schema = |reason: String| ContractError::Schema {
        file: RESULT_FILE,
        reason,
    };
    let obj = result
        .as_object()
        .ok_or_else(|| schema("must be a JSON object".to_string()))?;
    let mut gates = Vec::new();
    let mut names = BTreeSet::new();
    match obj.get("gates") {
        None => {}
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                if !item.is_object() {
                    return Err(schema(format!("gates[{i}] must be an object")));
                }
                let name = match item.get("name") {
                    Some(Value::String(s)) if !s.is_empty() => s.clone(),
                    _ => {
                        return Err(schema(format!(
                            "gates[{i}].name must be a non-empty string"
                        )))
                    }
                };
                let passed = match item.get("passed") {
                    Some(Value::Bool(b)) => *b,
                    _ => return Err(schema(format!("gates[{i}].passed must be a boolean"))),
                };
                if !names.insert(name.clone()) {
                    return Err(schema(format!("gate {name:?} appears twice")));
                }
                gates.push(Gate {
                    name,
                    passed,
                    body: item.clone(),
                });
            }
        }
        Some(_) => return Err(schema("`gates` must be an array".to_string())),
    }
    Ok((gates, result))
}

/// Best-effort `(campaign, cell)` of a directory, for keying its rejection.
pub fn campaign_and_cell_hint(dir: &Path) -> (String, String) {
    std::fs::read(dir.join(RUN_FILE))
        .ok()
        .and_then(|b| parse_run(&b).ok())
        .map(|(campaign, cell, _)| (campaign, cell))
        .unwrap_or_default()
}

/// Read and verify the run directory `dir`.
pub fn read_run_dir(dir: &Path) -> Result<RunContract, ContractError> {
    let run_bytes =
        read_contract_file(dir, RUN_FILE)?.ok_or(ContractError::MissingFile(RUN_FILE))?;
    let result_bytes =
        read_contract_file(dir, RESULT_FILE)?.ok_or(ContractError::MissingFile(RESULT_FILE))?;
    let mut present = Vec::new();
    for algorithm in [Algorithm::Sha256, Algorithm::Blake3] {
        if let Some(bytes) = read_contract_file(dir, algorithm.manifest_file())? {
            present.push((algorithm, bytes));
        }
    }
    if present.is_empty() {
        return Err(ContractError::MissingManifest);
    }
    let canonical_dir = dir.canonicalize().map_err(|e| ContractError::Unreadable {
        file: "run directory",
        reason: e.to_string(),
    })?;
    let mut manifests = Vec::new();
    for (algorithm, bytes) in &present {
        manifests.push(verify_manifest(dir, &canonical_dir, *algorithm, bytes)?);
    }
    // Parse only bytes a manifest attests: the copies read above must hash to
    // the listed digests, or a file swapped between the read and the
    // verification would be filed under a hash that does not cover it.
    for (algorithm, bytes) in &present {
        let entries = parse_manifest(
            algorithm.manifest_file(),
            std::str::from_utf8(bytes).unwrap_or(""),
        )?;
        for (file, read) in [(RUN_FILE, &run_bytes), (RESULT_FILE, &result_bytes)] {
            let listed = entries
                .iter()
                .find(|e| e.path == file)
                .map(|e| e.digest.as_str());
            if listed != Some(algorithm.digest_bytes(read).as_str()) {
                return Err(ContractError::Unreadable {
                    file,
                    reason: "changed while the run directory was being verified".to_string(),
                });
            }
        }
    }
    let (campaign, cell, run) = parse_run(&run_bytes)?;
    let (gates, result) = parse_result(&result_bytes)?;
    Ok(RunContract {
        campaign,
        cell,
        run,
        result,
        gates,
        manifests,
    })
}
