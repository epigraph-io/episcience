//! Shadow mode writes nothing to EpiGraph or EpiScience. The cheapest way to
//! keep that true is to keep the crate unable to: no database driver, no HTTP
//! client, no kernel or EpiScience crate in its dependency tables. A later
//! phase that adds a writer must do so in a separate crate or delete this test
//! on purpose.

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

#[test]
fn the_filer_crate_depends_on_no_writer() {
    let manifest =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
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
        }
    }
    assert!(
        deps.iter().any(|(_, d)| d == "serde_json"),
        "the dependency parser found nothing; it is broken: {deps:?}"
    );
    for (table, dep) in &deps {
        let dep_l = dep.to_ascii_lowercase();
        if table.starts_with("[dev-dependencies") {
            continue;
        }
        for bad in FORBIDDEN {
            assert!(
                !(dep_l == *bad || (bad.ends_with('-') && dep_l.starts_with(bad))),
                "{table} pulls in {dep}: shadow mode must not be able to write"
            );
        }
    }
}
