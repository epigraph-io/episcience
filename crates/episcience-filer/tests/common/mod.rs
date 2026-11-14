//! Synthetic run-directory fixtures. Campaign and cell names are invented.
#![allow(dead_code)]

use std::path::Path;

use episcience_filer::contract::Algorithm;

pub const RUN_A: &str = r#"{"campaign":"demo-campaign","cell":"cell-01","seed":7}"#;
pub const RESULT_A: &str = r#"{"status":"done","gates":[{"name":"energy_converged","passed":true,"value":0.5},{"name":"rmsd_below_cutoff","passed":false}]}"#;
pub const DATA_A: &str = "step,energy\n0,1.5\n1,0.5\n";

/// Write `files` into `dir` and a manifest for `algorithm` listing all of them.
pub fn write_run(dir: &Path, files: &[(&str, &str)], algorithm: Algorithm) {
    std::fs::create_dir_all(dir).unwrap();
    let mut manifest = String::new();
    for (name, body) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        manifest.push_str(&format!(
            "{}  {}\n",
            algorithm.digest_bytes(body.as_bytes()),
            name
        ));
    }
    std::fs::write(dir.join(algorithm.manifest_file()), manifest).unwrap();
}

/// The standard valid run: RUN.json, RESULT.json and one data file.
pub fn write_valid_run(dir: &Path, algorithm: Algorithm) {
    write_run(
        dir,
        &[
            ("RUN.json", RUN_A),
            ("RESULT.json", RESULT_A),
            ("data/energy.csv", DATA_A),
        ],
        algorithm,
    );
}

/// A valid run for `(campaign, cell)` with one passing gate.
pub fn write_cell(dir: &Path, campaign: &str, cell: &str) {
    let run = format!(r#"{{"campaign":"{campaign}","cell":"{cell}"}}"#);
    let result = r#"{"gates":[{"name":"ok","passed":true}]}"#;
    write_run(
        dir,
        &[("RUN.json", &run), ("RESULT.json", result)],
        Algorithm::Sha256,
    );
}

/// Flip the first byte of `path`.
pub fn flip_first_byte(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[0] ^= 0x01;
    std::fs::write(path, bytes).unwrap();
}
