//! Migration 5043's CHECKs on `blobs`: each one refuses the row it exists to
//! refuse, and the shapes the application writes are admitted. Each refused
//! row is an admitted shape with one change, and the refusal must name the
//! expected CHECK, so it cannot come from an unrelated constraint.
//!
//! Run through `scripts/e1-test-db.sh <batch> -- cargo test --test external_blobs_schema_test`.

mod support;

use support::{principal, Principal, TestDb};
use uuid::Uuid;

/// The columns a case sets; everything else is a valid public row.
#[derive(Clone)]
struct Row {
    storage: &'static str,
    location_uri: Option<String>,
    hash_algorithm: &'static str,
    verify_status: Option<&'static str>,
    verified: bool,
    observed: Option<Vec<u8>>,
    content_hash: Vec<u8>,
}

const HASH: [u8; 32] = [0xab; 32];

fn external() -> Row {
    Row {
        storage: "external",
        location_uri: Some("pvc://shared/run-7/traj.dcd".into()),
        hash_algorithm: "sha256",
        verify_status: None,
        verified: false,
        observed: None,
        content_hash: HASH.to_vec(),
    }
}

async fn insert(db: &TestDb, p: &Principal, r: &Row) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO blobs (id, filename, mime_type, size_bytes, content_hash, uploader_id, \
                            owner_group_id, visibility, storage, location_uri, hash_algorithm, \
                            verify_status, last_verified_at, last_verified_by, last_observed_hash) \
         VALUES ($1, 'traj.dcd', 'application/octet-stream', 10737418240, $2, $3, $4, 'public', \
                 $5, $6, $7, $8, CASE WHEN $9 THEN now() END, CASE WHEN $9 THEN $3 END, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(&r.content_hash[..])
    .bind(p.agent)
    .bind(p.personal_group)
    .bind(r.storage)
    .bind(r.location_uri.as_deref())
    .bind(r.hash_algorithm)
    .bind(r.verify_status)
    .bind(r.verified)
    .bind(r.observed.as_deref())
    .execute(&db.admin)
    .await
    .map(|_| ())
    .map_err(|e| match e {
        sqlx::Error::Database(d) => d.constraint().unwrap_or(d.message()).to_string(),
        other => other.to_string(),
    })
}

/// Every refusal names its CHECK (each refused row breaks exactly one, since
/// Postgres reports the first failing CHECK by name); every admitted shape is
/// accepted. Kills: any one of the nine CHECKs dropped or loosened (its
/// refused row is admitted, or refused by a different name), and a CHECK that
/// also refuses a shape the application writes (an admitted row fails).
#[tokio::test]
async fn each_5043_check_refuses_its_row_and_admits_its_twin() {
    let db = TestDb::fresh().await;
    let p = principal(&db.admin, "ext-blob-checks").await;

    let local = Row {
        storage: "local",
        location_uri: None,
        hash_algorithm: "blake3",
        ..external()
    };
    let ok = Row {
        verify_status: Some("ok"),
        verified: true,
        observed: Some(HASH.to_vec()),
        ..external()
    };
    let mismatch = Row {
        verify_status: Some("mismatch"),
        verified: true,
        observed: Some(vec![0xcd; 32]),
        ..external()
    };
    let unreachable = Row {
        verify_status: Some("unreachable"),
        verified: true,
        observed: None,
        ..external()
    };

    // The shapes the application writes.
    let admitted: Vec<(&str, Row)> = vec![
        ("local blake3", local.clone()),
        ("external sha256", external()),
        (
            "external blake3",
            Row {
                hash_algorithm: "blake3",
                ..external()
            },
        ),
        (
            "file URI with a host",
            Row {
                location_uri: Some("file://instrument-7/data/afm/map.ibw".into()),
                ..external()
            },
        ),
        (
            "s3 URI",
            Row {
                location_uri: Some("s3://bucket/key/with.dots+plus".into()),
                ..external()
            },
        ),
        (
            "https URI naming javascript:// later",
            Row {
                location_uri: Some("https://host/javascript://a".into()),
                ..external()
            },
        ),
        (
            "scheme that only starts with javascript",
            Row {
                location_uri: Some("javascripts://a".into()),
                ..external()
            },
        ),
        ("verified ok", ok.clone()),
        ("verified mismatch", mismatch.clone()),
        ("verified unreachable", unreachable.clone()),
    ];
    for (what, r) in &admitted {
        insert(&db, &p, r)
            .await
            .unwrap_or_else(|e| panic!("{what} must be admitted, refused by {e}"));
    }

    let refused: Vec<(&str, Row, &str)> = vec![
        (
            "unknown storage",
            Row {
                storage: "tape",
                location_uri: None,
                hash_algorithm: "blake3",
                ..external()
            },
            "blobs_storage_known",
        ),
        (
            // Not a 5043 CHECK: the legacy baseline's 32-byte rule covers
            // every row, so an external hash cannot be one `ok` never meets.
            "external with a 4-byte hash",
            Row {
                content_hash: vec![0xab; 4],
                ..external()
            },
            "blobs_content_hash_length",
        ),
        (
            "unknown algorithm",
            Row {
                hash_algorithm: "md5",
                ..external()
            },
            "blobs_hash_algorithm_known",
        ),
        (
            "external without a location",
            Row {
                location_uri: None,
                ..external()
            },
            "blobs_location_by_storage",
        ),
        (
            "local with a location",
            Row {
                location_uri: Some("pvc://shared/x".into()),
                ..local.clone()
            },
            "blobs_location_by_storage",
        ),
        (
            "location without a scheme",
            Row {
                location_uri: Some("/data/traj.dcd".into()),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "location with whitespace",
            Row {
                location_uri: Some("pvc://shared/a b".into()),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "location with a newline",
            Row {
                location_uri: Some("pvc://shared/a\nb".into()),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "javascript scheme",
            Row {
                location_uri: Some("javascript://x%0Aalert(1)".into()),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "vbscript scheme",
            Row {
                location_uri: Some("vbscript://x%0Amsgbox(1)".into()),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "data scheme",
            Row {
                location_uri: Some("data://text/html,x".into()),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "location past 2048 bytes",
            Row {
                location_uri: Some(format!("pvc://{}", "a".repeat(2044))),
                ..external()
            },
            "blobs_location_uri_shape",
        ),
        (
            "local sha256",
            Row {
                hash_algorithm: "sha256",
                ..local.clone()
            },
            "blobs_local_is_blake3",
        ),
        (
            "unknown status",
            Row {
                verify_status: Some("maybe"),
                ..unreachable.clone()
            },
            "blobs_verify_status_known",
        ),
        (
            "verified local blob",
            Row {
                storage: "local",
                location_uri: None,
                hash_algorithm: "blake3",
                ..unreachable.clone()
            },
            "blobs_verify_external_only",
        ),
        (
            "status without its time and verifier",
            Row {
                verified: false,
                ..unreachable.clone()
            },
            "blobs_verify_record_whole",
        ),
        (
            "time and verifier without a status",
            Row {
                verified: true,
                ..external()
            },
            "blobs_verify_record_whole",
        ),
        (
            "ok with another hash",
            Row {
                observed: Some(vec![0xcd; 32]),
                ..ok.clone()
            },
            "blobs_verify_observation",
        ),
        (
            "ok without an observed hash",
            Row {
                observed: None,
                ..ok.clone()
            },
            "blobs_verify_observation",
        ),
        (
            "mismatch with the stored hash",
            Row {
                observed: Some(HASH.to_vec()),
                ..mismatch.clone()
            },
            "blobs_verify_observation",
        ),
        (
            "mismatch with a short hash",
            Row {
                observed: Some(vec![0xcd; 16]),
                ..mismatch.clone()
            },
            "blobs_verify_observation",
        ),
        (
            "unreachable with an observed hash",
            Row {
                observed: Some(vec![0xcd; 32]),
                ..unreachable.clone()
            },
            "blobs_verify_observation",
        ),
        (
            "an observed hash and no status",
            Row {
                observed: Some(vec![0xcd; 32]),
                ..external()
            },
            "blobs_verify_observation",
        ),
    ];
    for (what, r, check) in &refused {
        let e = insert(&db, &p, r)
            .await
            .expect_err(&format!("{what} must be refused"));
        assert_eq!(e, *check, "{what}");
    }
}
