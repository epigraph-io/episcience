//! Shadow mode writes nothing to EpiGraph or EpiScience. This guard keeps the
//! obvious routes out of the crate: no database driver, HTTP or RPC client, or
//! kernel/EpiScience crate in its direct (non-dev) dependency tables, under any
//! rename, and no socket or child process in its source (`src/`, `src/bin/`
//! included). It is a guard, not a proof: a writer pulled in through another
//! dependency, or written by hand over a raw file descriptor, gets past it. A
//! later phase that adds a writer does so in a separate crate or changes this
//! test on purpose (docs/experiment-filer.md).
//!
//! Ported from #72 (backlog 28dde255).

use std::path::Path;

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
    "TcpListener",
    "UdpSocket",
    "unix::net",
    "UnixStream",
    "UnixDatagram",
    "process::Command",
    "Command::new",
];

/// The value of every `package = "<name>"` in `line`.
fn package_values(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("package") {
        rest = &rest[at + "package".len()..];
        if let Some(v) = rest.trim_start().strip_prefix('=') {
            if let Some(v) = v.trim_start().strip_prefix('"') {
                if let Some(end) = v.find('"') {
                    out.push(v[..end].to_string());
                }
            }
        }
    }
    out
}

/// `(table, crate name)` for every non-dev dependency of a Cargo.toml: the
/// key name AND, when present, the real `package = "..."` it renames.
fn dependency_names(manifest: &str) -> Vec<(String, String)> {
    let mut table = String::new();
    let mut deps = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            table = line.to_string();
            // `[dependencies.<name>]` / `[target.'...'.dependencies.<name>]`
            if let Some((_, name)) = line.trim_matches(['[', ']']).split_once("dependencies.") {
                deps.push((table.clone(), name.trim_matches('"').to_string()));
            }
            continue;
        }
        if table.contains("dependencies") && !line.is_empty() && !line.starts_with('#') {
            // `name = ...` or dotted `name.workspace = true`
            if line.contains('=') {
                let name = line.split(['=', '.']).next().unwrap_or("");
                deps.push((table.clone(), name.trim().trim_matches('"').to_string()));
            }
            for real in package_values(line) {
                deps.push((table.clone(), real));
            }
        }
    }
    deps.retain(|(table, _)| {
        !table.starts_with("[dev-dependencies") && !table.contains(".dev-dependencies")
    });
    deps
}

fn forbidden_in(deps: &[(String, String)]) -> Vec<String> {
    let mut hits = Vec::new();
    for (table, dep) in deps {
        let dep = dep.to_ascii_lowercase();
        for bad in FORBIDDEN {
            if dep == *bad || (bad.ends_with('-') && dep.starts_with(bad)) {
                hits.push(format!("{table} pulls in {dep}"));
            }
        }
    }
    hits
}

fn rust_sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_guard_sees_through_a_renamed_dependency() {
    let manifest = r#"
[dependencies]
serde_json.workspace = true
http = { package = "reqwest", version = "0.12" }

[dependencies.db]
package = "sqlx"
version = "0.8"

[target.'cfg(unix)'.dependencies]
kernel = { package = "epigraph-db", path = "../x" }

[dev-dependencies]
web = { package = "axum", version = "0.7" }
"#;
    let hits = forbidden_in(&dependency_names(manifest));
    assert_eq!(
        hits,
        [
            "[dependencies] pulls in reqwest",
            "[dependencies.db] pulls in sqlx",
            "[target.'cfg(unix)'.dependencies] pulls in epigraph-db",
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
    let src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut files = Vec::new();
    rust_sources(src, &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("bin/episcience-filer.rs")),
        "the scan must reach src/bin: {files:?}"
    );
    assert!(files.len() >= 9, "found only {} source files", files.len());
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        for bad in FORBIDDEN_SOURCE {
            assert!(!text.contains(bad), "{} uses {bad}", path.display());
        }
    }
}
