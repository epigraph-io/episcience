//! Manifest parsing and verification.
//!
//! Two formats, one line syntax (what `sha256sum` and `b3sum` print):
//!
//! ```text
//! <64 lowercase or uppercase hex digits><two spaces, or space + '*'><path>
//! ```
//!
//! `MANIFEST.sha256` carries SHA-256 digests, `MANIFEST.b3` BLAKE3 digests.
//! Not supported, and reported as malformed: the escaped form (`\` prefix,
//! for names with a newline or backslash) and BSD-style tagged lines.
//!
//! A manifest verifies when every line parses, every path stays inside the
//! run directory (no absolute path, no `..`, no symlink leading out), no path
//! is listed twice, and every listed file exists, is a regular file and has
//! the listed digest. Files present but not listed are reported separately
//! ([`ManifestReport::unlisted`]); the validator decides what they mean.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::contract::{read_contract_bytes, ContractError, MAX_MANIFEST_BYTES};
use crate::errors::FilerError;

pub const MANIFEST_SHA256: &str = "MANIFEST.sha256";
pub const MANIFEST_B3: &str = "MANIFEST.b3";

/// The digest a manifest carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    Sha256,
    Blake3,
}

impl HashAlgorithm {
    /// In preference order: when a run directory carries both manifests,
    /// both are verified and the first one present here is the primary (its
    /// hash is the run's content hash). SHA-256 first because the phase 1
    /// claim template is `checksum = sha256:<manifest hash>` (f376c374).
    pub const ALL: [Self; 2] = [Self::Sha256, Self::Blake3];

    #[must_use]
    pub const fn manifest_file(self) -> &'static str {
        match self {
            Self::Sha256 => MANIFEST_SHA256,
            Self::Blake3 => MANIFEST_B3,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }

    /// `<label>:<hex>`, the form every hash takes in the journal.
    #[must_use]
    pub fn tagged(self, hex_digest: &str) -> String {
        format!("{}:{hex_digest}", self.label())
    }

    /// Lowercase hex digest of `bytes`.
    #[must_use]
    pub fn hash_bytes(self, bytes: &[u8]) -> String {
        match self {
            Self::Sha256 => hex::encode(Sha256::digest(bytes)),
            Self::Blake3 => blake3::hash(bytes).to_hex().to_string(),
        }
    }

    /// Lowercase hex digest and length of everything `reader` yields,
    /// streamed (a data file is never held in memory).
    pub fn hash_reader(self, reader: &mut impl Read) -> io::Result<(String, u64)> {
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        let mut sha = Sha256::new();
        let mut b3 = blake3::Hasher::new();
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            match self {
                Self::Sha256 => sha.update(&buf[..n]),
                Self::Blake3 => {
                    b3.update(&buf[..n]);
                }
            }
            total += n as u64;
        }
        let digest = match self {
            Self::Sha256 => hex::encode(sha.finalize()),
            Self::Blake3 => b3.finalize().to_hex().to_string(),
        };
        Ok((digest, total))
    }
}

/// How a listed path leaves, or fails to name a file in, the run directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraversalKind {
    /// The path is absolute.
    Absolute,
    /// The path has a `..` component.
    ParentDir,
    /// The path resolves, through a symlink, outside the run directory.
    SymlinkEscape,
    /// The path names nothing (empty, or only `.` components).
    Empty,
}

/// One reason a manifest does not verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum ManifestIssue {
    /// A line is not `<digest>  <path>` (`line` 0: the file is not UTF-8).
    MalformedLine { line: usize, detail: String },
    /// The manifest lists no file.
    Empty,
    /// A listed path is absolute, has `..`, or escapes through a symlink.
    PathTraversal {
        line: usize,
        path: String,
        kind: TraversalKind,
    },
    /// A path is listed more than once.
    DuplicateEntry { line: usize, path: String },
    /// A listed file does not exist.
    MissingFile { path: String },
    /// A listed path is not a regular file.
    NotARegularFile { path: String },
    /// A listed file's digest differs from the manifest's.
    HashMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    /// A listed file, or a directory under the run directory, was unreadable.
    Io { path: String, detail: String },
}

/// One parsed manifest line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// 1-based line number.
    pub line: usize,
    /// Normalized relative path, `/`-separated, no `.` or `..` components.
    pub path: String,
    /// Lowercase hex digest.
    pub digest: String,
}

/// The outcome of verifying one manifest against its run directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestReport {
    pub algorithm: HashAlgorithm,
    /// `<label>:<hex>` over the manifest file's own bytes, with the
    /// manifest's own algorithm. This is the run's content hash: it commits
    /// to every file the manifest lists.
    pub manifest_hash: String,
    /// Listed path -> lowercase hex digest (first listing wins).
    pub entries: BTreeMap<String, String>,
    /// How many listed files matched their digest.
    pub verified: usize,
    /// Total size of the files that matched.
    pub total_bytes: u64,
    /// Why the manifest does not verify; empty when it does.
    pub issues: Vec<ManifestIssue>,
    /// Files under the run directory the manifest does not list, sorted.
    /// The manifest files themselves are exempt (a manifest cannot list
    /// itself); `RUN.json` and `RESULT.json` are not.
    pub unlisted: Vec<String>,
}

impl ManifestReport {
    /// Every listed file exists and matches, and nothing was malformed.
    #[must_use]
    pub fn verifies(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Normalize a manifest path: only plain components, joined with `/`.
pub fn normalize_rel_path(path: &str) -> Result<String, TraversalKind> {
    if path.contains('\0') {
        return Err(TraversalKind::Empty);
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => match part.to_str() {
                Some(part) => parts.push(part),
                None => return Err(TraversalKind::Empty),
            },
            Component::CurDir => {}
            Component::ParentDir => return Err(TraversalKind::ParentDir),
            Component::RootDir | Component::Prefix(_) => return Err(TraversalKind::Absolute),
        }
    }
    if parts.is_empty() {
        return Err(TraversalKind::Empty);
    }
    Ok(parts.join("/"))
}

fn parse_line(line: &str) -> Result<(String, &str), String> {
    if line.starts_with('\\') {
        return Err("escaped file names are not supported".into());
    }
    let bytes = line.as_bytes();
    if bytes.len() < 67 {
        return Err("expected `<64 hex digits>  <path>`".into());
    }
    if !bytes[..64].iter().all(u8::is_ascii_hexdigit) {
        return Err("the digest is not 64 hex digits".into());
    }
    if bytes[64] != b' ' || (bytes[65] != b' ' && bytes[65] != b'*') {
        return Err("expected two spaces (or ` *`) between digest and path".into());
    }
    // Bytes 0..66 are ASCII, so both slices fall on character boundaries.
    Ok((line[..64].to_ascii_lowercase(), &line[66..]))
}

/// Parse manifest text. Blank lines are skipped; a trailing `\r` is dropped.
#[must_use]
pub fn parse_manifest(text: &str) -> (Vec<ManifestEntry>, Vec<ManifestIssue>) {
    let mut entries = Vec::new();
    let mut issues = Vec::new();
    for (idx, raw) in text.split('\n').enumerate() {
        let line_no = idx + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            continue;
        }
        match parse_line(line) {
            Ok((digest, path)) => match normalize_rel_path(path) {
                Ok(normalized) => entries.push(ManifestEntry {
                    line: line_no,
                    path: normalized,
                    digest,
                }),
                Err(kind) => issues.push(ManifestIssue::PathTraversal {
                    line: line_no,
                    path: path.to_string(),
                    kind,
                }),
            },
            Err(detail) => issues.push(ManifestIssue::MalformedLine {
                line: line_no,
                detail,
            }),
        }
    }
    (entries, issues)
}

/// Where a relative path leads, from a canonical run directory.
pub(crate) enum Resolved {
    /// A regular file inside the run directory (its canonical path).
    File(PathBuf),
    Missing,
    /// It resolves outside the run directory.
    Escapes,
    NotAFile,
    Io(String),
}

/// Resolve `rel` under the CANONICAL directory `root`, following symlinks,
/// and refuse anything that ends up outside `root`.
pub(crate) fn resolve_inside(root: &Path, rel: &str) -> Resolved {
    match fs::canonicalize(root.join(rel)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Resolved::Missing,
        Err(e) => Resolved::Io(e.to_string()),
        Ok(real) => {
            if !real.starts_with(root) {
                return Resolved::Escapes;
            }
            match fs::metadata(&real) {
                Ok(meta) if meta.is_file() => Resolved::File(real),
                Ok(_) => Resolved::NotAFile,
                Err(e) => Resolved::Io(e.to_string()),
            }
        }
    }
}

/// A directory entry's name as journal text: `Ok(name)` when it is UTF-8,
/// `Err(rendering)` when it is not. The rendering is injective over non-UTF-8
/// names: `\` is doubled and each byte that is not part of valid UTF-8 is
/// written `\xNN`. A UTF-8 name is never escaped (journaled paths keep their
/// spelling), so a rendering can equal a UTF-8 name that is spelled with a
/// literal `\x`; the caller therefore never matches a rendering against a
/// listed path, and a non-UTF-8 directory below the watched root is filed with
/// its own rejection reason ([`crate::validate::Rejection::UnscannableDirectory`]).
pub(crate) fn utf8_name(name: &OsStr) -> Result<String, String> {
    if let Some(name) = name.to_str() {
        return Ok(name.to_string());
    }
    let mut bytes = name.as_encoded_bytes();
    let mut out = String::new();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(valid) => {
                out.push_str(&valid.replace('\\', "\\\\"));
                return Err(out);
            }
            Err(e) => {
                let (valid, rest) = bytes.split_at(e.valid_up_to());
                // `valid` is UTF-8 by construction (`valid_up_to`).
                out.push_str(&String::from_utf8_lossy(valid).replace('\\', "\\\\"));
                let bad = e.error_len().unwrap_or(rest.len());
                for b in &rest[..bad] {
                    out.push_str(&format!("\\x{b:02x}"));
                }
                bytes = &rest[bad..];
            }
        }
    }
}

/// Collect the files under `dir` that no manifest line lists. `listable` is
/// `false` below a directory whose name is not UTF-8: no manifest line can
/// name such a path, so everything there is unlisted.
fn walk_unlisted(
    dir: &Path,
    prefix: &str,
    listable: bool,
    listed: &BTreeMap<String, (usize, String)>,
    unlisted: &mut Vec<String>,
    issues: &mut Vec<ManifestIssue>,
) {
    let io_issue = |e: io::Error| ManifestIssue::Io {
        path: if prefix.is_empty() {
            ".".to_string()
        } else {
            prefix.to_string()
        },
        detail: e.to_string(),
    };
    let reader = match fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(e) => {
            issues.push(io_issue(e));
            return;
        }
    };
    // (rendered name, whether it is UTF-8, real path, is a directory)
    let mut children: Vec<(String, bool, PathBuf, bool)> = Vec::new();
    for entry in reader {
        match entry.and_then(|e| e.file_type().map(|t| (e.file_name(), e.path(), t.is_dir()))) {
            // `file_type` does not follow symlinks: a symlink is reported as
            // a file under its own name and never descended into. The walk
            // recurses on the entry's real path, never on a rendered name.
            Ok((name, path, is_dir)) => {
                let (rendered, utf8) = match utf8_name(&name) {
                    Ok(name) => (name, true),
                    Err(rendered) => (rendered, false),
                };
                children.push((rendered, utf8, path, is_dir));
            }
            Err(e) => issues.push(io_issue(e)),
        }
    }
    children.sort();
    for (name, utf8, path, is_dir) in children {
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let listable = listable && utf8;
        if is_dir {
            walk_unlisted(&path, &rel, listable, listed, unlisted, issues);
        } else if !listable
            || (!listed.contains_key(&rel)
                && !(prefix.is_empty() && (name == MANIFEST_SHA256 || name == MANIFEST_B3)))
        {
            unlisted.push(rel);
        }
    }
}

/// Verify manifest `bytes` against the CANONICAL run directory `root`.
pub(crate) fn verify_manifest_bytes(
    root: &Path,
    algorithm: HashAlgorithm,
    bytes: &[u8],
) -> ManifestReport {
    let manifest_hash = algorithm.tagged(&algorithm.hash_bytes(bytes));
    let mut issues = Vec::new();
    // path -> (line, digest)
    let mut listed: BTreeMap<String, (usize, String)> = BTreeMap::new();
    match std::str::from_utf8(bytes) {
        Err(_) => issues.push(ManifestIssue::MalformedLine {
            line: 0,
            detail: "the manifest is not valid UTF-8".into(),
        }),
        Ok(text) => {
            let (parsed, mut parse_issues) = parse_manifest(text);
            issues.append(&mut parse_issues);
            for entry in parsed {
                match listed.entry(entry.path) {
                    Entry::Occupied(slot) => issues.push(ManifestIssue::DuplicateEntry {
                        line: entry.line,
                        path: slot.key().clone(),
                    }),
                    Entry::Vacant(slot) => {
                        slot.insert((entry.line, entry.digest));
                    }
                }
            }
        }
    }
    if listed.is_empty() && issues.is_empty() {
        issues.push(ManifestIssue::Empty);
    }

    let mut verified = 0usize;
    let mut total_bytes = 0u64;
    for (path, (line, expected)) in &listed {
        let real = match resolve_inside(root, path) {
            Resolved::File(real) => real,
            Resolved::Missing => {
                issues.push(ManifestIssue::MissingFile { path: path.clone() });
                continue;
            }
            Resolved::Escapes => {
                issues.push(ManifestIssue::PathTraversal {
                    line: *line,
                    path: path.clone(),
                    kind: TraversalKind::SymlinkEscape,
                });
                continue;
            }
            Resolved::NotAFile => {
                issues.push(ManifestIssue::NotARegularFile { path: path.clone() });
                continue;
            }
            Resolved::Io(detail) => {
                issues.push(ManifestIssue::Io {
                    path: path.clone(),
                    detail,
                });
                continue;
            }
        };
        match fs::File::open(&real).and_then(|mut file| algorithm.hash_reader(&mut file)) {
            Ok((actual, len)) if actual == *expected => {
                verified += 1;
                total_bytes += len;
            }
            Ok((actual, _)) => issues.push(ManifestIssue::HashMismatch {
                path: path.clone(),
                expected: expected.clone(),
                actual,
            }),
            Err(e) => issues.push(ManifestIssue::Io {
                path: path.clone(),
                detail: e.to_string(),
            }),
        }
    }

    let mut unlisted = Vec::new();
    walk_unlisted(root, "", true, &listed, &mut unlisted, &mut issues);

    ManifestReport {
        algorithm,
        manifest_hash,
        entries: listed
            .into_iter()
            .map(|(path, (_, digest))| (path, digest))
            .collect(),
        verified,
        total_bytes,
        issues,
        unlisted,
    }
}

/// Verify the `algorithm` manifest of `run_dir`.
///
/// `Err` only when the run directory or the manifest file cannot be read at
/// all; everything else is in the report.
pub fn verify_manifest(
    run_dir: &Path,
    algorithm: HashAlgorithm,
) -> Result<ManifestReport, FilerError> {
    let root = fs::canonicalize(run_dir).map_err(|e| FilerError::io(run_dir, e))?;
    let manifest_path = root.join(algorithm.manifest_file());
    let bytes = read_contract_bytes(&manifest_path, MAX_MANIFEST_BYTES).map_err(|e| {
        let detail = match e {
            ContractError::Unreadable(d) | ContractError::Invalid(d) => d,
        };
        FilerError::io(
            &manifest_path,
            io::Error::new(io::ErrorKind::InvalidData, detail),
        )
    })?;
    Ok(verify_manifest_bytes(&root, algorithm, &bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Published test vectors for the three-byte input "abc".
    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const ABC_BLAKE3: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";

    #[test]
    fn known_answers_for_both_algorithms() {
        assert_eq!(HashAlgorithm::Sha256.hash_bytes(b"abc"), ABC_SHA256);
        assert_eq!(HashAlgorithm::Blake3.hash_bytes(b"abc"), ABC_BLAKE3);
        let (sha, n) = HashAlgorithm::Sha256.hash_reader(&mut &b"abc"[..]).unwrap();
        assert_eq!((sha.as_str(), n), (ABC_SHA256, 3));
        let (b3, n) = HashAlgorithm::Blake3.hash_reader(&mut &b"abc"[..]).unwrap();
        assert_eq!((b3.as_str(), n), (ABC_BLAKE3, 3));
    }

    #[test]
    fn parses_text_and_binary_markers_and_uppercase_digests() {
        let text = format!(
            "{ABC_SHA256}  data/a.txt\n{} *./b.bin\r\n\n",
            ABC_SHA256.to_ascii_uppercase()
        );
        let (entries, issues) = parse_manifest(&text);
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "data/a.txt");
        assert_eq!(entries[1].path, "b.bin");
        assert_eq!(entries[1].digest, ABC_SHA256);
        assert_eq!(entries[1].line, 2);
    }

    #[test]
    fn refuses_malformed_lines() {
        let (entries, issues) = parse_manifest(&format!(
            "not a manifest line\n{ABC_SHA256} one-space\n\\{ABC_SHA256}  esc\n"
        ));
        assert!(entries.is_empty());
        assert_eq!(issues.len(), 3);
        assert!(issues
            .iter()
            .all(|i| matches!(i, ManifestIssue::MalformedLine { .. })));
    }

    #[test]
    fn a_path_listed_twice_and_an_empty_manifest_do_not_verify() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("abc.txt"), b"abc").unwrap();
        std::fs::write(
            tmp.path().join(MANIFEST_SHA256),
            format!("{ABC_SHA256}  abc.txt\n{ABC_SHA256}  ./abc.txt\n"),
        )
        .unwrap();
        let report = verify_manifest(tmp.path(), HashAlgorithm::Sha256).unwrap();
        assert_eq!(
            report.issues,
            vec![ManifestIssue::DuplicateEntry {
                line: 2,
                path: "abc.txt".into()
            }]
        );
        assert_eq!(report.verified, 1);

        std::fs::write(tmp.path().join(MANIFEST_SHA256), "\n").unwrap();
        let report = verify_manifest(tmp.path(), HashAlgorithm::Sha256).unwrap();
        assert_eq!(report.issues, vec![ManifestIssue::Empty]);
        assert_eq!(report.unlisted, vec!["abc.txt".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_renders_injectively() {
        use std::os::unix::ffi::OsStrExt;
        let name = |b: &[u8]| utf8_name(OsStr::from_bytes(b));
        assert_eq!(name(b"cell-01"), Ok("cell-01".to_string()));
        // A UTF-8 name keeps its spelling, backslash included.
        assert_eq!(name(br"a\b"), Ok(r"a\b".to_string()));
        assert_eq!(name(b"a\xff"), Err(r"a\xff".to_string()));
        // `\` is doubled, so these two cannot render alike.
        assert_eq!(name(b"a\\\xff"), Err(r"a\\\xff".to_string()));
        assert_eq!(name(b"a\\xff\xff"), Err(r"a\\xff\xff".to_string()));
        // A truncated multi-byte sequence, then valid text.
        assert_eq!(name(b"\xe2\x82z"), Err(r"\xe2\x82z".to_string()));
    }

    #[test]
    fn normalizes_and_refuses_paths() {
        assert_eq!(normalize_rel_path("./a//b/./c").unwrap(), "a/b/c");
        assert_eq!(normalize_rel_path("/etc/x"), Err(TraversalKind::Absolute));
        assert_eq!(
            normalize_rel_path("a/../../x"),
            Err(TraversalKind::ParentDir)
        );
        assert_eq!(normalize_rel_path(".."), Err(TraversalKind::ParentDir));
        assert_eq!(normalize_rel_path("./."), Err(TraversalKind::Empty));
        assert_eq!(normalize_rel_path(""), Err(TraversalKind::Empty));
    }
}
