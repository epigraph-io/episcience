//! Ratchet R7 (first needles, batch E1a): the request path takes its
//! principal ONLY from the validated token's `agent_id`.
//!
//! A static scan of `episcience-api/src`. It fails when:
//! - `unwrap_or(claims.sub)` (or any `.sub` fallback spelling) reappears: the
//!   OAuth client id is not an agent;
//! - `auth_agent_id` (the retired server-wide MCP identity) reappears anywhere;
//! - `EPIGRAPH_SERVICE_AGENT_ID` appears outside the exact register below
//!   (the boot refusal every binary shares, `config::RETIRED_SERVICE_VARS`);
//! - the retired service client reappears: its types (`ServiceToken`,
//!   `EpigraphEdgesClient`, `EpigraphEventsClient`) anywhere, its modules
//!   (`src/clients/{service_token,epigraph_edges,epigraph_events}.rs`, deleted
//!   in E1h) at all, or `EPIGRAPH_CLIENT_ID` outside the register below.
//!
//! The register only shrinks; E1h brought it to its final form (the shared
//! boot refusal only).
//!
//! Final E1g needles (the request path on stamped sessions):
//! - every route registered with `post` / `patch` / `delete` goes through
//!   `write_as`, except the exact exemption register below (each a read
//!   carried by POST, or a retired 410 that touches nothing), and every other
//!   route handler reads through `read_as`;
//! - every MCP WRITE tool (`claims:write` in `auth::scopes`) takes the
//!   caller's auth context and viewer, and its handler writes through
//!   `write_as`;
//! - no route or MCP tool opens a transaction any other way (`.begin(`).

use std::path::{Path, PathBuf};

const FORBIDDEN_EVERYWHERE: &[&str] = &[
    "unwrap_or(claims.sub)",
    "or(Some(claims.sub))",
    "auth_agent_id",
    // The retired service client (E1f), deleted in E1h.
    "ServiceToken",
    "EpigraphEdgesClient",
    "EpigraphEventsClient",
];

/// The retired service client's modules (deleted in E1h): none may come back.
const RETIRED_MODULES: &[&str] = &[
    "src/clients/service_token.rs",
    "src/clients/epigraph_edges.rs",
    "src/clients/epigraph_events.rs",
];

/// `(needle, files allowed to contain it)`.
///
/// E1f retired the service client and E1h deleted its modules; its
/// variables and the retired service identity are named in ONE place, the
/// boot refusal every binary runs (`config::RETIRED_SERVICE_VARS`). Final
/// (E1h): nothing else in `src` may name them.
const REGISTER: &[(&str, &[&str])] = &[
    ("EPIGRAPH_SERVICE_AGENT_ID", &["src/config.rs"]),
    ("EPIGRAPH_CLIENT_ID", &["src/config.rs"]),
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

/// The retired service client's modules are gone and stay gone (E1h). Kills:
/// a module restored under its old path (its types are refused everywhere by
/// `FORBIDDEN_EVERYWHERE`; this catches the file itself, and a `mod`
/// declaration that would compile it).
#[test]
fn the_retired_client_modules_are_gone() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for m in RETIRED_MODULES {
        assert!(!root.join(m).exists(), "{m} is back");
    }
    let clients = std::fs::read_to_string(root.join("src/clients/mod.rs")).expect("clients/mod.rs");
    for m in ["service_token", "epigraph_edges", "epigraph_events"] {
        assert!(
            !clients.contains(&format!("mod {m}")),
            "clients/mod.rs declares {m}"
        );
    }
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

// ─── E1g: write routes and write tools go through `write_as` ────────────────

/// `(handler, why)`: a handler registered with a mutating method that is not
/// a write (exact; a stale entry fails `write_route_exemptions_are_live`).
const WRITE_ROUTE_EXEMPT: &[(&str, &str)] = &[
    (
        "search",
        "POST /syntheses/search: a read (the query rides in the body); runs on read_as",
    ),
    (
        "shares_retired",
        "the retired share routes: 410, no database access at all",
    ),
];

/// Handlers that touch no database at all (`health::check`, the retired
/// share routes; neither `read_as` nor
/// `write_as` applies).
const NO_DATABASE: &[&str] = &["check", "shares_retired"];

/// The body of `fn name` (from its signature to the next top-level `fn` or
/// the end of the file). Source scan only.
fn fn_body<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(&format!("fn {name}("))?;
    let rest = &text[start..];
    let end = rest[1..]
        .find("\nasync fn ")
        .into_iter()
        .chain(rest[1..].find("\nfn "))
        .chain(rest[1..].find("\npub fn "))
        .chain(rest[1..].find("\npub async fn "))
        .min()
        .map_or(rest.len(), |i| i + 1);
    Some(&rest[..end])
}

/// `(method, handler)` for every route registration in `text`, including
/// chained ones (`post(a).get(b)`).
fn registered(text: &str) -> Vec<(String, String)> {
    let re = regex::Regex::new(r"\b(get|post|patch|delete|put)\((\w+)\)").unwrap();
    re.captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

#[test]
fn every_write_route_goes_through_write_as() {
    let mut problems = Vec::new();
    let mut writes = 0;
    for (rel, text) in sources() {
        if !rel.starts_with("src/routes/") {
            continue;
        }
        if text.contains(".begin(") {
            problems.push(format!("{rel}: opens a transaction outside write_as"));
        }
        for (method, handler) in registered(&text) {
            let body = fn_body(&text, &handler)
                .unwrap_or_else(|| panic!("{rel}: handler {handler} not found"));
            let exempt = WRITE_ROUTE_EXEMPT.iter().any(|(h, _)| *h == handler);
            if method != "get" && !exempt {
                writes += 1;
                if !body.contains(".write_as(") {
                    problems.push(format!("{rel}: {method}({handler}) does not use write_as"));
                }
            } else if !NO_DATABASE.contains(&handler.as_str()) && !body.contains(".read_as(") {
                problems.push(format!(
                    "{rel}: {method}({handler}) does not read through read_as"
                ));
            }
        }
    }
    assert!(writes >= 10, "the scan found the write routes: {writes}");
    assert!(problems.is_empty(), "R7 (E1g):\n{}", problems.join("\n"));
}

#[test]
fn write_route_exemptions_are_live() {
    let all: Vec<(String, String)> = sources()
        .iter()
        .filter(|(rel, _)| rel.starts_with("src/routes/"))
        .flat_map(|(_, t)| registered(t))
        .collect();
    for (h, _) in WRITE_ROUTE_EXEMPT {
        assert!(
            all.iter().any(|(m, x)| x == h && m != "get"),
            "{h} is no longer registered with a mutating method: shrink the register"
        );
    }
}

/// The MCP write tools, from the scope table (the one list of tools and
/// what they need), and the module file each dispatches to.
#[test]
fn every_mcp_write_tool_takes_the_caller_and_writes_through_write_as() {
    use episcience_api::auth::scopes::{CLAIMS_WRITE, MCP_TOOL_SCOPES};
    let src = sources();
    let get = |rel: &str| {
        src.iter()
            .find(|(r, _)| r == rel)
            .unwrap_or_else(|| panic!("{rel} missing"))
            .1
            .clone()
    };
    let module = get("src/mcp/mod.rs");
    let dispatch = regex::Regex::new(r"(\w+)::handle\(self, &auth, &viewer, args\)").unwrap();
    let mut problems = Vec::new();
    let write_tools: Vec<&str> = MCP_TOOL_SCOPES
        .iter()
        .filter(|(_, s)| *s == CLAIMS_WRITE)
        .map(|(t, _)| *t)
        .collect();
    assert_eq!(write_tools.len(), 5, "{write_tools:?}");
    for tool in write_tools {
        let body = fn_body(&module, tool).unwrap_or_else(|| panic!("tool {tool} not found"));
        if !body.contains("caller(&extensions)?") {
            problems.push(format!("{tool}: does not take the resolved caller"));
            continue;
        }
        let Some(m) = dispatch.captures(body) else {
            problems.push(format!("{tool}: does not pass the auth context and viewer"));
            continue;
        };
        let file = get(&format!("src/mcp/{}.rs", &m[1]));
        let handle = fn_body(&file, "handle").expect("handle");
        if !handle.contains("    auth: &AuthContext,") {
            problems.push(format!("{tool}: its handler ignores the auth context"));
        }
        if !handle.contains(".write_as(") {
            problems.push(format!(
                "{tool}: its handler does not write through write_as"
            ));
        }
        if file.contains(".begin(") {
            problems.push(format!("{tool}: opens a transaction outside write_as"));
        }
    }
    assert!(problems.is_empty(), "R7 (E1g):\n{}", problems.join("\n"));
}
