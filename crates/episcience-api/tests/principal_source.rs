//! Ratchet R7 (first needles, batch E1a): the request path takes its
//! principal ONLY from the validated token's `agent_id`.
//!
//! A static scan of `episcience-api/src`. It fails when:
//! - `unwrap_or(claims.sub)` (or any `.sub` fallback spelling) reappears: the
//!   OAuth client id is not an agent;
//! - `auth_agent_id` (the retired server-wide MCP identity) reappears anywhere;
//! - `EPIGRAPH_SERVICE_AGENT_ID` appears outside the exact register below
//!   (today: only the MCP binary's boot warning that the variable is ignored).
//!
//! The register only shrinks; later batches remove the last entry.

use std::path::{Path, PathBuf};

const FORBIDDEN_EVERYWHERE: &[&str] = &[
    "unwrap_or(claims.sub)",
    "or(Some(claims.sub))",
    "auth_agent_id",
];

/// `(needle, files allowed to contain it)`.
const REGISTER: &[(&str, &[&str])] = &[(
    "EPIGRAPH_SERVICE_AGENT_ID",
    &["src/bin/episcience-mcp-server.rs"],
)];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    assert!(
        files.len() > 20,
        "scan found too few files: {}",
        files.len()
    );
    files
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (rel, std::fs::read_to_string(&p).expect("read source"))
        })
        .collect()
}

#[test]
fn no_principal_fallback_or_service_identity_in_src() {
    let mut hits = Vec::new();
    for (rel, text) in sources() {
        for needle in FORBIDDEN_EVERYWHERE {
            if text.contains(needle) {
                hits.push(format!("{rel}: {needle}"));
            }
        }
        for (needle, allowed) in REGISTER {
            if text.contains(needle) && !allowed.contains(&rel.as_str()) {
                hits.push(format!("{rel}: {needle} (not in the register)"));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "principal-source needles found:\n{}",
        hits.join("\n")
    );
}

/// The register is exact: every allowed entry is still present, so a stale
/// entry cannot silently widen what the ratchet tolerates.
#[test]
fn register_entries_are_live() {
    let sources = sources();
    for (needle, allowed) in REGISTER {
        for file in *allowed {
            let text = &sources
                .iter()
                .find(|(rel, _)| rel == file)
                .unwrap_or_else(|| panic!("register names a missing file {file}"))
                .1;
            assert!(
                text.contains(needle),
                "{file} no longer contains {needle}: shrink the register"
            );
        }
    }
}
