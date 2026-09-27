//! Boot-time configuration checks shared by the two binaries.
//!
//! Every check here runs BEFORE the database is touched, so a misconfigured
//! process exits at once and never connects, migrates, or starts a worker.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Name of the shared HMAC secret the kernel signs access tokens with.
pub const JWT_SECRET_VAR: &str = "EPIGRAPH_JWT_SECRET";

/// Address the REST server binds (an IP literal). Default: loopback.
pub const BIND_ADDR_VAR: &str = "EPISCIENCE_BIND_ADDR";

/// Port the REST server binds. Default: [`DEFAULT_PORT`].
pub const PORT_VAR: &str = "EPISCIENCE_PORT";

/// Historical default REST port when `EPISCIENCE_PORT` is unset.
pub const DEFAULT_PORT: u16 = 8081;

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

/// The REST listen address from `EPISCIENCE_BIND_ADDR` and `EPISCIENCE_PORT`.
///
/// The address defaults to `127.0.0.1` and must be an IP literal. The
/// wildcard addresses (`0.0.0.0`, `::`) are REFUSED: binding every interface
/// is never the intended exposure. A deployment that must be reachable from a
/// specific non-loopback interface names that interface's address.
pub fn rest_bind_addr(bind: Option<&str>, port: Option<&str>) -> Result<SocketAddr, String> {
    let ip = match bind.map(str::trim).filter(|s| !s.is_empty()) {
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Some(raw) => raw
            .parse::<IpAddr>()
            .map_err(|_| format!("{BIND_ADDR_VAR}={raw} is not an IP address"))?,
    };
    if ip.is_unspecified() {
        return Err(format!(
            "{BIND_ADDR_VAR}={ip} is refused: binding every interface is not allowed; \
             use 127.0.0.1 (the default) or the specific address a client needs"
        ));
    }
    let port = match port.map(str::trim).filter(|s| !s.is_empty()) {
        None => DEFAULT_PORT,
        Some(raw) => raw
            .parse::<u16>()
            .map_err(|_| format!("{PORT_VAR}={raw} is not a port number"))?,
    };
    Ok(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_defaults_to_loopback_and_refuses_wildcards() {
        assert_eq!(
            rest_bind_addr(None, Some("8092")).unwrap(),
            "127.0.0.1:8092".parse().unwrap()
        );
        assert_eq!(rest_bind_addr(None, None).unwrap().port(), DEFAULT_PORT);
        for wildcard in ["0.0.0.0", "::", "0:0:0:0:0:0:0:0"] {
            let err = rest_bind_addr(Some(wildcard), Some("8092")).unwrap_err();
            assert!(err.contains("refused"), "{wildcard}: {err}");
        }
        // A specific non-loopback address is allowed.
        assert_eq!(
            rest_bind_addr(Some("192.0.2.10"), Some("8092")).unwrap(),
            "192.0.2.10:8092".parse().unwrap()
        );
        assert_eq!(
            rest_bind_addr(Some("::1"), Some("8092")).unwrap(),
            "[::1]:8092".parse().unwrap()
        );
        assert!(rest_bind_addr(Some("localhost"), None).is_err());
        assert!(rest_bind_addr(None, Some("eighty")).is_err());
    }

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
