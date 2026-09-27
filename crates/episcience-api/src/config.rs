//! Boot-time configuration checks shared by the two binaries.
//!
//! Every check here runs BEFORE the database is touched, so a misconfigured
//! process exits at once and never connects, migrates, or starts a worker.

/// Name of the shared HMAC secret the kernel signs access tokens with.
pub const JWT_SECRET_VAR: &str = "EPIGRAPH_JWT_SECRET";

/// The shared token secret, or an error when it is unset or empty.
///
/// There is deliberately NO fallback: a process without the secret must not
/// start, rather than verify tokens against a publicly known development key.
pub fn require_jwt_secret(value: Option<String>) -> Result<Vec<u8>, String> {
    match value {
        Some(s) if !s.is_empty() => Ok(s.into_bytes()),
        _ => Err(format!(
            "{JWT_SECRET_VAR} must be set: EpiScience verifies kernel-minted access tokens \
             with it and has no development fallback"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_is_required_and_never_defaulted() {
        assert!(require_jwt_secret(None).is_err());
        assert!(require_jwt_secret(Some(String::new())).is_err());
        assert_eq!(
            require_jwt_secret(Some("s3cret".into())).unwrap(),
            b"s3cret".to_vec()
        );
    }
}
