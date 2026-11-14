//! Shadow mode writes nothing to EpiGraph or EpiScience. This ratchet keeps
//! the obvious routes out of the crate: no database driver, HTTP client or
//! kernel/EpiScience crate in its direct dependency tables (under any rename),
//! and no socket or `process::Command` in its source. It is a
//! guard, not a proof: a writer pulled in through another dependency, or
//! written by hand over a raw file descriptor, gets past it. A later phase
//! that adds a writer must do so in a separate crate or change this test on
//! purpose.

const FORBIDDEN: &[&str] = &[
    "sqlx",
    "postgres",
    "tokio-postgres",
    "reqwest",
    "hyper",
    "ureq",
    "axum",
    "rmcp",
    "epigraph-",
    "episcience-",
];

const FORBIDDEN_SOURCE: &[&str] = &[
    "std::net",
    "TcpStream",
    "UdpSocket",
    "process::Command",
    "Command::new",
];

/// The value of every `package = "<name>"` in `line`.
fn package_values(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("package") {
        let after = rest[at + "package".len()..].trim_start();
        rest = &rest[at + "package".len()..];
        if let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            if let Some(v) = v.strip_prefix('"') {
                if let Some(end) = v.find('"') {
                    out.push(v[..end].to_string());
                }
            }
        }
    }
    out
}

/// `(table, crate name)` for every non-dev dependency of a Cargo.toml: the
/// key name AND, when present, the real `package = "…"` it renames.
fn dependency_names(manifest: &str) -> Vec<(String, String)> {
    let mut table = String::new();
    let mut deps = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            table = line.to_string();
            // `[dependencies.<name>]` / `[target.'…'.dependencies.<name>]`
            if let Some((_, name)) = line.trim_matches(['[', ']']).split_once("dependencies.") {
                deps.push((table.clone(), name.trim_matches('"').to_string()));
            }
            continue;
        }
        if table.contains("dependencies") && !line.is_empty() && !line.starts_with('#') {
            // `name = …` or dotted `name.workspace = true`
            if line.contains('=') {
                let name = line.split(['=', '.']).next().unwrap_or("");
                deps.push((table.clone(), name.trim().trim_matches('"').to_string()));
            }
            for real in package_values(line) {
                deps.push((table.clone(), real));
            }
        }
    }
    deps.retain(|(table, _)| !table.starts_with("[dev-dependencies"));
    deps
}

fn forbidden_in(deps: &[(String, String)]) -> Vec<String> {
    let mut hits = Vec::new();
    for (table, dep) in deps {
        let dep_l = dep.to_ascii_lowercase();
        for bad in FORBIDDEN {
            if dep_l == *bad || (bad.ends_with('-') && dep_l.starts_with(bad)) {
                hits.push(format!("{table} pulls in {dep}"));
            }
        }
    }
    hits
}

#[test]
fn the_ratchet_sees_through_a_renamed_dependency() {
    let manifest = r#"
[dependencies]
serde_json.workspace = true
http = { package = "reqwest", version = "0.12" }

[dependencies.db]
package = "sqlx"
version = "0.8"

[dev-dependencies]
web = { package = "axum", version = "0.7" }
"#;
    let hits = forbidden_in(&dependency_names(manifest));
    assert_eq!(
        hits,
        [
            "[dependencies] pulls in reqwest",
            "[dependencies.db] pulls in sqlx"
        ],
        "a dev-dependency is allowed; a renamed writer is not"
    );
}

#[test]
fn the_filer_crate_depends_on_no_writer() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let deps = dependency_names(&manifest);
    assert!(
        deps.iter().any(|(_, d)| d == "serde_json"),
        "the dependency parser found nothing; it is broken: {deps:?}"
    );
    let hits = forbidden_in(&deps);
    assert!(
        hits.is_empty(),
        "shadow mode must not be able to write: {hits:?}"
    );
}

#[test]
fn the_filer_source_opens_no_socket_and_spawns_no_process() {
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut checked = 0;
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            for bad in FORBIDDEN_SOURCE {
                assert!(!text.contains(bad), "{} uses {bad}", path.display());
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 5,
        "found only {checked} source files under {src}"
    );
}
