use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Metadata reference to a blob.
///
/// A `local` blob's bytes live at
/// `EPISCIENCE_BLOB_DIR/{hash_hex[0:2]}/{hash_hex[2:4]}/{hash_hex}.blob`, under
/// the BLAKE3 hash the server computed. An `external` blob's bytes stay where
/// they are, at `location_uri`; its `content_hash` was DECLARED by the
/// registering caller (with `hash_algorithm`), so it must never be used to
/// read the local blob directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobRef {
    pub id: Uuid,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub content_hash: Vec<u8>,
    pub uploader_id: Uuid,
    pub sample_id: Option<Uuid>,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// The owning group (kernel `groups.id`) and who may read the row
    /// (`None` only for a legacy row before the one-shot re-own).
    pub owner_group_id: Option<Uuid>,
    pub visibility: Option<crate::synthesis::Visibility>,
    /// Where the bytes are (migration 5043; `local` for every earlier row).
    #[serde(default)]
    pub storage: BlobStorage,
    /// Where an external blob's bytes live; `None` for a local blob.
    #[serde(default)]
    pub location_uri: Option<String>,
    /// The algorithm of `content_hash` (`blake3` for every local blob).
    #[serde(default)]
    pub hash_algorithm: HashAlgorithm,
    /// The blob is a manifest (a list of files and their hashes).
    #[serde(default)]
    pub is_manifest: bool,
    /// The latest in-place verification of an external blob (`None`: never
    /// verified, and always for a local blob).
    #[serde(default)]
    pub verify_status: Option<VerifyStatus>,
    #[serde(default)]
    pub last_verified_at: Option<DateTime<Utc>>,
    /// The agent that reported the latest verification.
    #[serde(default)]
    pub last_verified_by: Option<Uuid>,
    /// The hash the verifier observed (kept for `ok` and `mismatch`).
    #[serde(default)]
    pub last_observed_hash: Option<Vec<u8>>,
}

impl BlobRef {
    /// Compute the filesystem path for a LOCAL blob's content. `None` for an
    /// external blob: its hash is caller-declared and names nothing here.
    pub fn storage_path(&self, base_dir: &std::path::Path) -> Option<std::path::PathBuf> {
        if self.storage != BlobStorage::Local {
            return None;
        }
        assert!(
            self.content_hash.len() >= 4,
            "content_hash must be at least 4 bytes"
        );
        let hex = hex::encode(&self.content_hash);
        Some(
            base_dir
                .join(&hex[0..2])
                .join(&hex[2..4])
                .join(format!("{}.blob", hex)),
        )
    }
}

/// Where a blob's bytes are (`blobs.storage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlobStorage {
    /// In the blob directory, uploaded through EpiScience.
    #[default]
    Local,
    /// At `location_uri`, registered by URI and hash; no bytes uploaded.
    External,
}

impl BlobStorage {
    pub fn as_str(self) -> &'static str {
        match self {
            BlobStorage::Local => "local",
            BlobStorage::External => "external",
        }
    }
}

impl std::str::FromStr for BlobStorage {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "local" => Ok(BlobStorage::Local),
            "external" => Ok(BlobStorage::External),
            _ => Err(format!("unknown blob storage: {s}")),
        }
    }
}

/// The algorithm of a blob's `content_hash` (`blobs.hash_algorithm`). Both
/// produce 32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    #[default]
    Blake3,
    Sha256,
}

impl HashAlgorithm {
    pub fn as_str(self) -> &'static str {
        match self {
            HashAlgorithm::Blake3 => "blake3",
            HashAlgorithm::Sha256 => "sha256",
        }
    }
}

impl std::str::FromStr for HashAlgorithm {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "blake3" => Ok(HashAlgorithm::Blake3),
            "sha256" => Ok(HashAlgorithm::Sha256),
            _ => Err(format!(
                "unknown hash_algorithm: {s} (expected blake3 or sha256)"
            )),
        }
    }
}

/// The outcome of an external blob's latest in-place verification
/// (`blobs.verify_status`). Computed by the server from the row's own hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStatus {
    /// The bytes at the location hash to `content_hash`.
    Ok,
    /// They hash to something else (kept in `last_observed_hash`).
    Mismatch,
    /// The verifier could not read them.
    Unreachable,
}

impl VerifyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyStatus::Ok => "ok",
            VerifyStatus::Mismatch => "mismatch",
            VerifyStatus::Unreachable => "unreachable",
        }
    }
}

impl std::str::FromStr for VerifyStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ok" => Ok(VerifyStatus::Ok),
            "mismatch" => Ok(VerifyStatus::Mismatch),
            "unreachable" => Ok(VerifyStatus::Unreachable),
            _ => Err(format!("unknown verify_status: {s}")),
        }
    }
}

/// The longest `location_uri` accepted (migration 5043's CHECK).
pub const MAX_LOCATION_URI_BYTES: usize = 2048;

/// Validate an external blob's location: `<scheme>://<rest>`, the scheme a
/// lowercase letter then `[a-z0-9+.-]`, the rest non-empty with no
/// whitespace or control character, at most [`MAX_LOCATION_URI_BYTES`].
/// The same rule as migration 5043's `blobs_location_uri_shape` CHECK (the
/// CHECK is the backstop; this gives the caller a 422 instead).
///
/// # Errors
/// A message naming what is wrong.
pub fn validate_location_uri(uri: &str) -> Result<(), String> {
    if uri.len() > MAX_LOCATION_URI_BYTES {
        return Err(format!(
            "location_uri is {} bytes (max {MAX_LOCATION_URI_BYTES})",
            uri.len()
        ));
    }
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| "location_uri must be <scheme>://<location>".to_string())?;
    let mut chars = scheme.chars();
    let scheme_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+.-".contains(c));
    if !scheme_ok {
        return Err(format!(
            "location_uri scheme {scheme:?} must be a lowercase letter followed by [a-z0-9+.-]"
        ));
    }
    if rest.is_empty() {
        return Err("location_uri names no location after the scheme".into());
    }
    if rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("location_uri must not contain whitespace or control characters".into());
    }
    Ok(())
}

/// Parse a 32-byte hash given as 64 hex digits (either case).
///
/// # Errors
/// A message naming `field` when the text is not 64 hex digits.
pub fn parse_hash_hex(field: &str, text: &str) -> Result<Vec<u8>, String> {
    let t = text.trim();
    if t.len() != 64 {
        return Err(format!(
            "{field} must be 64 hex digits (32 bytes), got {} characters",
            t.len()
        ));
    }
    hex::decode(t).map_err(|e| format!("{field} is not hex: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_uri_rule_matches_the_check() {
        for ok in [
            "pvc://shared/run-1/traj.dcd",
            "s3://bucket/key/with.dots+plus",
            "file://instrument-7/data/map.ibw",
            "x-y.z+1://a",
        ] {
            assert_eq!(validate_location_uri(ok), Ok(()), "{ok}");
        }
        for bad in [
            "/data/traj.dcd",
            "pvc:/shared",
            "PVC://shared",
            "1pvc://shared",
            "pv_c://shared",
            "pvc://",
            "pvc://a b",
            "pvc://a\nb",
            "pvc://a\u{7}b",
        ] {
            assert!(
                validate_location_uri(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        let at_cap = format!("pvc://{}", "a".repeat(MAX_LOCATION_URI_BYTES - 6));
        assert_eq!(validate_location_uri(&at_cap), Ok(()));
        let past = format!("{at_cap}a");
        assert!(validate_location_uri(&past).is_err());
    }

    #[test]
    fn hash_hex_is_exactly_32_bytes() {
        let h = "ab".repeat(32);
        assert_eq!(parse_hash_hex("h", &h).unwrap(), vec![0xab; 32]);
        assert_eq!(
            parse_hash_hex("h", &h.to_uppercase()).unwrap(),
            vec![0xab; 32]
        );
        assert!(parse_hash_hex("h", &"ab".repeat(31)).is_err());
        assert!(parse_hash_hex("h", &"ab".repeat(33)).is_err());
        assert!(parse_hash_hex("h", &"zz".repeat(32)).is_err());
    }

    #[test]
    fn an_external_blob_has_no_local_path() {
        let mut b: BlobRef = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(), "filename": "f", "mime_type": "x/y", "size_bytes": 1,
            "content_hash": vec![0xabu8; 32], "uploader_id": Uuid::nil(), "sample_id": null,
            "labels": [], "properties": {}, "created_at": "2026-11-14T00:00:00Z",
            "owner_group_id": null, "visibility": null
        }))
        .expect("a pre-5043 BlobRef still deserializes");
        assert_eq!(b.storage, BlobStorage::Local);
        assert_eq!(b.hash_algorithm, HashAlgorithm::Blake3);
        assert!(b.storage_path(std::path::Path::new("/b")).is_some());
        b.storage = BlobStorage::External;
        assert_eq!(b.storage_path(std::path::Path::new("/b")), None);
    }
}
