//! The run-contract reader and manifest verifier.

mod common;

use common::*;
use episcience_filer::contract::{
    parse_manifest, read_run_dir, Algorithm, ContractError, MANIFEST_SHA256,
    MAX_CONTRACT_FILE_BYTES,
};

#[test]
fn a_complete_sha256_run_reads_with_its_fields_and_gates_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    let c = read_run_dir(tmp.path()).expect("valid run");
    assert_eq!(c.campaign, "demo-campaign");
    assert_eq!(c.cell, "cell-01");
    assert_eq!(c.run["seed"], 7);
    assert_eq!(c.gates.len(), 2);
    assert_eq!(c.gates[0].name, "energy_converged");
    assert!(c.gates[0].passed);
    assert_eq!(c.gates[0].body["value"], 0.5);
    assert!(!c.gates[1].passed);
    assert_eq!(c.manifests.len(), 1);
    assert_eq!(c.manifests[0].entries, 3);
    // The manifest hash is the SHA-256 of the manifest file's own bytes.
    let manifest = std::fs::read(tmp.path().join(MANIFEST_SHA256)).unwrap();
    assert_eq!(
        c.primary_manifest_hash(),
        format!("sha256:{}", Algorithm::Sha256.digest_bytes(&manifest))
    );
}

#[test]
fn a_complete_b3_run_reads_and_its_hash_is_blake3() {
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Blake3);
    let c = read_run_dir(tmp.path()).expect("valid run");
    assert_eq!(c.manifests[0].algorithm, Algorithm::Blake3);
    assert!(c.primary_manifest_hash().starts_with("blake3:"));
}

#[test]
fn sha256sum_output_verifies_including_binary_mode_lines() {
    // Digests below are from `sha256sum`, not from this crate:
    //   printf '%s' '{"campaign":"c","cell":"x"}' | sha256sum
    //   printf '%s' '{}' | sha256sum
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("RUN.json"),
        r#"{"campaign":"c","cell":"x"}"#,
    )
    .unwrap();
    std::fs::write(tmp.path().join("RESULT.json"), "{}").unwrap();
    std::fs::write(
        tmp.path().join(MANIFEST_SHA256),
        "SHA_RUN  ./RUN.json\nSHA_RESULT *RESULT.json\n"
            .replace(
                "SHA_RUN",
                "ca804f79f2dc2709d180a76a13de3f3aa0c4c53180ddb85ebc1da2f9707be761",
            )
            .replace(
                "SHA_RESULT",
                "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
            ),
    )
    .unwrap();
    let c = read_run_dir(tmp.path()).expect("sha256sum-format manifest verifies");
    assert!(c.gates.is_empty());
}

#[test]
fn each_missing_contract_file_rejects_the_run() {
    for (gone, expected) in [
        ("RUN.json", ContractError::MissingFile("RUN.json")),
        ("RESULT.json", ContractError::MissingFile("RESULT.json")),
        (MANIFEST_SHA256, ContractError::MissingManifest),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write_valid_run(tmp.path(), Algorithm::Sha256);
        std::fs::remove_file(tmp.path().join(gone)).unwrap();
        assert_eq!(
            read_run_dir(tmp.path()).unwrap_err(),
            expected,
            "without {gone}"
        );
    }
}

#[test]
fn one_flipped_byte_in_any_listed_file_is_a_digest_mismatch() {
    for victim in ["data/energy.csv", "RUN.json", "RESULT.json"] {
        for algorithm in [Algorithm::Sha256, Algorithm::Blake3] {
            let tmp = tempfile::tempdir().unwrap();
            write_valid_run(tmp.path(), algorithm);
            flip_first_byte(&tmp.path().join(victim));
            match read_run_dir(tmp.path()) {
                Err(ContractError::DigestMismatch { path, .. }) => assert_eq!(path, victim),
                other => panic!("{victim} ({algorithm:?}) tampered: {other:?}"),
            }
        }
    }
}

#[test]
fn a_listed_file_that_is_absent_rejects_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    std::fs::remove_file(tmp.path().join("data/energy.csv")).unwrap();
    assert!(matches!(
        read_run_dir(tmp.path()),
        Err(ContractError::ListedFileUnreadable { path, .. }) if path == "data/energy.csv"
    ));
}

#[test]
fn both_manifests_are_verified_when_both_are_present() {
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    // A b3 manifest that lists RUN.json and RESULT.json correctly but carries
    // a wrong digest for the data file.
    let b3 = format!(
        "{}  RUN.json\n{}  RESULT.json\n{}  data/energy.csv\n",
        Algorithm::Blake3.digest_bytes(RUN_A.as_bytes()),
        Algorithm::Blake3.digest_bytes(RESULT_A.as_bytes()),
        Algorithm::Blake3.digest_bytes(b"something else"),
    );
    std::fs::write(tmp.path().join("MANIFEST.b3"), b3).unwrap();
    assert!(matches!(
        read_run_dir(tmp.path()),
        Err(ContractError::DigestMismatch {
            file: "MANIFEST.b3",
            ..
        })
    ));
}

#[test]
fn a_manifest_must_list_run_and_result() {
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    let manifest = std::fs::read_to_string(tmp.path().join(MANIFEST_SHA256)).unwrap();
    let without_result: String = manifest
        .lines()
        .filter(|l| !l.ends_with("RESULT.json"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(tmp.path().join(MANIFEST_SHA256), without_result).unwrap();
    assert_eq!(
        read_run_dir(tmp.path()).unwrap_err(),
        ContractError::NotListed {
            file: MANIFEST_SHA256,
            path: "RESULT.json"
        }
    );
}

#[test]
fn manifest_path_and_syntax_rules() {
    let d = "0".repeat(64);
    let bad = [
        (format!("{d}  ../escape"), "contains `..`"),
        (format!("{d}  /etc/passwd"), "is absolute"),
        (format!("{d}  a\n{d}  ./a"), "is listed twice"),
        (
            format!("{d}  MANIFEST.b3"),
            "a manifest cannot attest a manifest",
        ),
    ];
    for (text, reason) in bad {
        match parse_manifest(MANIFEST_SHA256, &text) {
            Err(ContractError::ManifestPath { reason: r, .. }) => {
                assert!(r.contains(reason), "{text:?}: {r}")
            }
            other => panic!("{text:?}: {other:?}"),
        }
    }
    let syntax = [
        format!("{}  a", "0".repeat(63)),
        format!("{}  a", "A".repeat(64)),
        format!("{d} a"),
        format!("\\{d}  a"),
    ];
    for text in syntax {
        assert!(
            matches!(
                parse_manifest(MANIFEST_SHA256, &text),
                Err(ContractError::ManifestSyntax { .. })
            ),
            "{text:?}"
        );
    }
    assert_eq!(
        parse_manifest(MANIFEST_SHA256, "\n\n").unwrap_err(),
        ContractError::EmptyManifest {
            file: MANIFEST_SHA256
        }
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_run_directory_is_rejected() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), DATA_A).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    let link = tmp.path().join("data/energy.csv");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), &link).unwrap();
    match read_run_dir(tmp.path()) {
        Err(ContractError::ListedFileUnreadable { reason, .. }) => {
            assert!(reason.contains("outside"))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn run_and_result_schema_rules() {
    let cases = [
        (
            r#"{"cell":"x"}"#,
            "{}",
            "`campaign` must be a non-empty string",
        ),
        (
            r#"{"campaign":"","cell":"x"}"#,
            "{}",
            "`campaign` must be a non-empty string",
        ),
        (
            r#"{"campaign":"c"}"#,
            "{}",
            "`cell` must be a non-empty string",
        ),
        (r#"["campaign"]"#, "{}", "must be a JSON object"),
        (
            r#"{"campaign":"c","cell":"x"}"#,
            r#"{"gates":{}}"#,
            "`gates` must be an array",
        ),
        (
            r#"{"campaign":"c","cell":"x"}"#,
            r#"{"gates":[{"passed":true}]}"#,
            "gates[0].name",
        ),
        (
            r#"{"campaign":"c","cell":"x"}"#,
            r#"{"gates":[{"name":"g","passed":"yes"}]}"#,
            "gates[0].passed",
        ),
        (
            r#"{"campaign":"c","cell":"x"}"#,
            r#"{"gates":[{"name":"g","passed":true},{"name":"g","passed":false}]}"#,
            "appears twice",
        ),
    ];
    for (run, result, reason) in cases {
        let tmp = tempfile::tempdir().unwrap();
        write_run(
            tmp.path(),
            &[("RUN.json", run), ("RESULT.json", result)],
            Algorithm::Sha256,
        );
        match read_run_dir(tmp.path()) {
            Err(ContractError::Schema { reason: r, .. }) => {
                assert!(r.contains(reason), "{run} {result}: {r}")
            }
            other => panic!("{run} {result}: {other:?}"),
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    write_run(
        tmp.path(),
        &[("RUN.json", "{not json"), ("RESULT.json", "{}")],
        Algorithm::Sha256,
    );
    assert!(matches!(
        read_run_dir(tmp.path()),
        Err(ContractError::InvalidJson {
            file: "RUN.json",
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn a_contract_file_must_be_a_regular_file_not_a_symlink() {
    // RUN.json -> a file INSIDE the run directory with the same bytes: the
    // manifest digest still matches, but a contract file that is a symlink
    // (which could point at /dev/zero or a FIFO) is refused before any read.
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    std::fs::write(tmp.path().join("run-copy.json"), RUN_A).unwrap();
    std::fs::remove_file(tmp.path().join("RUN.json")).unwrap();
    std::os::unix::fs::symlink("run-copy.json", tmp.path().join("RUN.json")).unwrap();
    match read_run_dir(tmp.path()) {
        Err(ContractError::Unreadable {
            file: "RUN.json",
            reason,
        }) => {
            assert!(reason.contains("not a regular file"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_oversized_contract_file_is_refused_without_reading_it_whole() {
    let tmp = tempfile::tempdir().unwrap();
    write_valid_run(tmp.path(), Algorithm::Sha256);
    // A sparse file one byte over the cap.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(tmp.path().join("RESULT.json"))
        .unwrap();
    f.set_len(MAX_CONTRACT_FILE_BYTES + 1).unwrap();
    match read_run_dir(tmp.path()) {
        Err(ContractError::Unreadable {
            file: "RESULT.json",
            reason,
        }) => {
            assert!(reason.contains("larger than"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
}
