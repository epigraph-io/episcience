//! Ratchet R7 (first needles, batch E1a): the request path takes its
//! principal ONLY from the validated token's `agent_id`.
//!
//! A static scan of `episcience-api/src`. It fails when:
//! - `unwrap_or(claims.sub)` (or any `.sub` fallback spelling) reappears: the
//!   OAuth client id is not an agent;
//! - `auth_agent_id` (the retired server-wide MCP identity) reappears anywhere;
//! - `EPIGRAPH_SERVICE_AGENT_ID` appears outside the exact register below
//!   (the MCP binary's boot warning that the variable is ignored, and the
//!   worker's boot refusal);
//! - the retired service client (E1f: `ServiceToken`, `EpigraphEdgesClient`,
//!   `EpigraphEventsClient`, `EPIGRAPH_CLIENT_ID`) appears outside its dead
//!   modules, the ignored-variable warnings and the worker's refusal: no
//!   binary, job or route constructs it.
//!
//! The register only shrinks; later batches remove the last entry.

use std::path::{Path, PathBuf};

const FORBIDDEN_EVERYWHERE: &[&str] = &[
    "unwrap_or(claims.sub)",
    "or(Some(claims.sub))",
    "auth_agent_id",
];

/// `(needle, files allowed to contain it)`.
///
/// E1f: the service client is retired. Its types stay only in the dead
/// `src/clients/` modules (deleted in E1h), and its variables are named only
/// by the two binaries' "set but ignored" warnings and the worker's boot
/// refusals (`config.rs`' refusal list, the worker binary's doc).
const REGISTER: &[(&str, &[&str])] = &[
    (
        "EPIGRAPH_SERVICE_AGENT_ID",
        &[
            "src/bin/episcience-mcp-server.rs",
            "src/config.rs",
            "src/bin/episcience-worker.rs",
        ],
    ),
    (
        "EPIGRAPH_CLIENT_ID",
        &[
            "src/bin/episcience-mcp-server.rs",
            "src/bin/server.rs",
            "src/config.rs",
            "src/bin/episcience-worker.rs",
        ],
    ),
    (
        "ServiceToken",
        &[
            "src/clients/service_token.rs",
            "src/clients/epigraph_edges.rs",
            "src/clients/epigraph_events.rs",
        ],
    ),
    (
        "EpigraphEdgesClient",
        &[
            "src/clients/epigraph_edges.rs",
            "src/clients/epigraph_events.rs",
        ],
    ),
    ("EpigraphEventsClient", &["src/clients/epigraph_events.rs"]),
];

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
