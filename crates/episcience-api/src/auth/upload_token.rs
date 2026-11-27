//! Out-of-band blob upload tokens (backlog efc5549c / 4a1abad4).
//!
//! MCP `attach_blob` can take a file's bytes inline only as base64, through
//! the model's context. In upload mode it instead declares the file (its
//! SHA-256, size and metadata) and receives a short-lived token; the bytes go
//! straight to `PUT /api/v1/eln/blobs/upload` with that token.
//!
//! A token is a signed [`UploadGrant`]:
//!
//! ```text
//! v1.<base64url(grant JSON)>.<base64url(HMAC-SHA256(key, "episcience-upload-v1." || grant JSON))>
//! ```
//!
//! The grant binds the uploading agent, the sample (if any), the declared
//! SHA-256 and size, the blob's metadata and the blob id it will be stored
//! under, and expires within [`MAX_TOKEN_LIFETIME_SECS`]. Single use is the
//! provisional ruling's option a: no server-side token state. A replay inside
//! the lifetime can only re-send the same bytes, by the same agent, to the
//! same sample, under the same blob id, which the route treats as idempotent.
//!
//! The key is [`UPLOAD_TOKEN_KEY_VAR`], shared by the REST and MCP processes.
//! Unset, the feature is off: MCP issues no token and the route refuses every
//! upload.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::sync::Arc;
use uuid::Uuid;

/// The HMAC key of upload tokens. Both the REST server (which accepts them)
/// and the MCP server (which issues them) read it; unset, neither does.
pub const UPLOAD_TOKEN_KEY_VAR: &str = "EPISCIENCE_UPLOAD_TOKEN_KEY";

/// The REST server's externally reachable origin (e.g. `https://eln.example`),
/// which MCP puts in front of the upload path. Unset, MCP returns the path
/// alone.
pub const UPLOAD_BASE_URL_VAR: &str = "EPISCIENCE_UPLOAD_BASE_URL";

/// Shortest accepted key, as for the kernel's shared token secret.
pub const MIN_KEY_BYTES: usize = 32;

/// Longest lifetime a token may have (10 minutes). A token whose expiry is
/// further out than this is refused even with a valid MAC.
pub const MAX_TOKEN_LIFETIME_SECS: i64 = 600;

/// Largest grant (JSON bytes). The token travels in a request header, and
/// proxies commonly refuse header lines past 8 KiB; base64 adds a third.
pub const MAX_GRANT_BYTES: usize = 4096;

/// The header that carries the token on the upload route.
pub const UPLOAD_TOKEN_HEADER: &str = "x-upload-token";

/// The upload route's path.
pub const UPLOAD_PATH: &str = "/api/v1/eln/blobs/upload";

const VERSION: &str = "v1";
const DOMAIN: &[u8] = b"episcience-upload-v1.";

/// What a token allows: one upload of exactly these bytes, by this agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UploadGrant {
    /// The id the blob is stored under (a replay finds the same row).
    pub blob_id: Uuid,
    /// The only agent whose bearer may use the token.
    pub agent_id: Uuid,
    /// The sample the blob attaches to; write access is rechecked at upload.
    pub sample_id: Option<Uuid>,
    /// SHA-256 of the bytes, 64 lowercase hex digits.
    pub sha256: String,
    /// Exact size of the bytes.
    pub size_bytes: u64,
    /// Expiry, Unix seconds.
    pub exp: i64,
    pub filename: String,
    pub mime_type: String,
    pub labels: Vec<String>,
    pub properties: serde_json::Value,
}

/// Why a token was refused. The route answers each with the same 403 text,
/// so a caller learns nothing about which check failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenRefusal {
    #[error("malformed upload token")]
    Malformed,
    #[error("upload token signature does not verify")]
    BadSignature,
    #[error("upload token expired")]
    Expired,
    #[error("upload token lifetime exceeds the maximum")]
    LifetimeTooLong,
}

/// The HMAC key. `Debug` never prints it.
#[derive(Clone)]
pub struct UploadTokenKey(Arc<[u8]>);

impl std::fmt::Debug for UploadTokenKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UploadTokenKey(<redacted>)")
    }
}

type HmacSha256 = Hmac<Sha256>;

impl UploadTokenKey {
    /// A key of at least [`MIN_KEY_BYTES`] bytes.
    ///
    /// # Errors
    /// A shorter key.
    pub fn new(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < MIN_KEY_BYTES {
            return Err(format!(
                "{UPLOAD_TOKEN_KEY_VAR} is refused: {} bytes, at least {MIN_KEY_BYTES} required",
                bytes.len()
            ));
        }
        Ok(Self(Arc::from(bytes)))
    }

    /// The key from the environment value: `None` when unset or empty (the
    /// feature is off), an error (refuse boot) when set but too short.
    ///
    /// # Errors
    /// A set key shorter than [`MIN_KEY_BYTES`].
    pub fn from_env_value(value: Option<String>) -> Result<Option<Self>, String> {
        match value {
            Some(v) if !v.is_empty() => Self::new(v.as_bytes()).map(Some),
            _ => Ok(None),
        }
    }

    fn mac(&self, payload: &[u8]) -> HmacSha256 {
        let mut mac = HmacSha256::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(DOMAIN);
        mac.update(payload);
        mac
    }

    /// Sign `grant` as it is (the caller sets `exp`).
    ///
    /// # Errors
    /// A grant larger than [`MAX_GRANT_BYTES`].
    pub fn sign(&self, grant: &UploadGrant) -> Result<String, String> {
        let payload = serde_json::to_vec(grant).map_err(|e| format!("encode grant: {e}"))?;
        if payload.len() > MAX_GRANT_BYTES {
            return Err(format!(
                "the upload's metadata is too large for a token: {} bytes of grant, at most \
                 {MAX_GRANT_BYTES} (shorten filename, labels or properties, or send the bytes inline)",
                payload.len()
            ));
        }
        let tag = self.mac(&payload).finalize().into_bytes();
        Ok(format!(
            "{VERSION}.{}.{}",
            B64URL.encode(&payload),
            B64URL.encode(tag)
        ))
    }

    /// The grant a token carries, if its MAC verifies, it has not expired at
    /// `now` (Unix seconds) and its expiry is no more than
    /// [`MAX_TOKEN_LIFETIME_SECS`] after `now`.
    ///
    /// # Errors
    /// The [`TokenRefusal`].
    pub fn verify(&self, token: &str, now: i64) -> Result<UploadGrant, TokenRefusal> {
        let mut parts = token.trim().split('.');
        let (Some(version), Some(payload_b64), Some(tag_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(TokenRefusal::Malformed);
        };
        if version != VERSION {
            return Err(TokenRefusal::Malformed);
        }
        let payload = B64URL
            .decode(payload_b64)
            .map_err(|_| TokenRefusal::Malformed)?;
        let tag = B64URL
            .decode(tag_b64)
            .map_err(|_| TokenRefusal::Malformed)?;
        // Constant-time comparison, before the payload is parsed.
        self.mac(&payload)
            .verify_slice(&tag)
            .map_err(|_| TokenRefusal::BadSignature)?;
        let grant: UploadGrant =
            serde_json::from_slice(&payload).map_err(|_| TokenRefusal::Malformed)?;
        if grant.exp <= now {
            return Err(TokenRefusal::Expired);
        }
        if grant.exp.saturating_sub(now) > MAX_TOKEN_LIFETIME_SECS {
            return Err(TokenRefusal::LifetimeTooLong);
        }
        Ok(grant)
    }
}

/// A declared SHA-256: 64 hex digits, returned lowercase.
///
/// # Errors
/// Anything else.
pub fn normalize_sha256_hex(text: &str) -> Result<String, String> {
    let t = text.trim();
    if t.len() != 64 || !t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "content_hash must be the SHA-256 of the file as 64 hex digits, got {} characters",
            t.len()
        ));
    }
    Ok(t.to_ascii_lowercase())
}

/// The upload URL MCP hands out: `base` (trailing `/` trimmed) and
/// [`UPLOAD_PATH`], or the path alone without a base.
#[must_use]
pub fn upload_url(base: Option<&str>) -> String {
    match base.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => format!("{}{UPLOAD_PATH}", b.trim_end_matches('/')),
        None => UPLOAD_PATH.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn key() -> UploadTokenKey {
        UploadTokenKey::new(&[7u8; 32]).unwrap()
    }

    fn grant(exp: i64) -> UploadGrant {
        UploadGrant {
            blob_id: Uuid::now_v7(),
            agent_id: Uuid::now_v7(),
            sample_id: Some(Uuid::now_v7()),
            sha256: "ab".repeat(32),
            size_bytes: 42,
            exp,
            filename: "gel.png".into(),
            mime_type: "image/png".into(),
            labels: vec!["gel".into()],
            properties: serde_json::json!({"lane": 3}),
        }
    }

    #[test]
    fn a_signed_grant_verifies_to_itself() {
        let g = grant(NOW + 300);
        let t = key().sign(&g).unwrap();
        assert_eq!(key().verify(&t, NOW).unwrap(), g);
    }

    // Kills: verify skipping the MAC (a holder could rewrite the declared
    // hash, the agent or the sample).
    #[test]
    fn a_rewritten_grant_is_refused() {
        let g = grant(NOW + 300);
        let t = key().sign(&g).unwrap();
        let mut forged = g.clone();
        forged.sha256 = "cd".repeat(32);
        let payload = B64URL.encode(serde_json::to_vec(&forged).unwrap());
        let parts: Vec<&str> = t.split('.').collect();
        let tampered = format!("{}.{}.{}", parts[0], payload, parts[2]);
        assert_eq!(
            key().verify(&tampered, NOW),
            Err(TokenRefusal::BadSignature)
        );
    }

    #[test]
    fn another_key_does_not_verify() {
        let t = key().sign(&grant(NOW + 300)).unwrap();
        let other = UploadTokenKey::new(&[8u8; 32]).unwrap();
        assert_eq!(other.verify(&t, NOW), Err(TokenRefusal::BadSignature));
    }

    // Kills: the expiry check removed or made inclusive of `now`.
    #[test]
    fn an_expired_token_is_refused() {
        let t = key().sign(&grant(NOW)).unwrap();
        assert_eq!(key().verify(&t, NOW), Err(TokenRefusal::Expired));
        let t = key().sign(&grant(NOW - 1)).unwrap();
        assert_eq!(key().verify(&t, NOW), Err(TokenRefusal::Expired));
    }

    // Kills: the lifetime cap removed (a minting bug, or a leaked key used
    // to mint long-lived tokens, would otherwise be accepted).
    #[test]
    fn a_token_living_past_ten_minutes_is_refused() {
        let ok = key().sign(&grant(NOW + MAX_TOKEN_LIFETIME_SECS)).unwrap();
        assert!(key().verify(&ok, NOW).is_ok());
        let long = key()
            .sign(&grant(NOW + MAX_TOKEN_LIFETIME_SECS + 1))
            .unwrap();
        assert_eq!(key().verify(&long, NOW), Err(TokenRefusal::LifetimeTooLong));
    }

    #[test]
    fn malformed_tokens_are_refused() {
        let t = key().sign(&grant(NOW + 300)).unwrap();
        let body = t.strip_prefix("v1.").unwrap();
        for bad in [
            String::new(),
            "v1".to_string(),
            format!("v2.{body}"),
            format!("{t}.extra"),
            "v1.!!!.!!!".to_string(),
        ] {
            assert!(key().verify(&bad, NOW).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn short_or_unset_keys() {
        assert!(UploadTokenKey::new(&[1u8; 31]).is_err());
        assert!(UploadTokenKey::from_env_value(None).unwrap().is_none());
        assert!(UploadTokenKey::from_env_value(Some(String::new()))
            .unwrap()
            .is_none());
        assert!(UploadTokenKey::from_env_value(Some("short".into())).is_err());
        assert!(UploadTokenKey::from_env_value(Some("k".repeat(32)))
            .unwrap()
            .is_some());
        assert_eq!(format!("{:?}", key()), "UploadTokenKey(<redacted>)");
    }

    #[test]
    fn an_oversized_grant_is_not_signed() {
        let mut g = grant(NOW + 300);
        g.properties = serde_json::json!({"blob": "x".repeat(MAX_GRANT_BYTES)});
        assert!(key().sign(&g).is_err());
    }

    #[test]
    fn sha256_hex_and_upload_url() {
        assert_eq!(
            normalize_sha256_hex(&"AB".repeat(32)).unwrap(),
            "ab".repeat(32)
        );
        assert!(normalize_sha256_hex(&"ab".repeat(31)).is_err());
        assert!(normalize_sha256_hex(&"zz".repeat(32)).is_err());
        assert_eq!(upload_url(None), UPLOAD_PATH);
        assert_eq!(
            upload_url(Some("https://eln.example/")),
            "https://eln.example/api/v1/eln/blobs/upload"
        );
    }
}
