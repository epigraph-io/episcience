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

/// `true` for every spelling of the all-interfaces address: `0.0.0.0`, `::`
/// and the IPv4-mapped `::ffff:0.0.0.0` (an `AF_INET6` socket bound to the
/// latter listens on every IPv4 interface). The address is canonicalised
/// first, so an IPv4-mapped IPv6 literal is judged as the IPv4 address it
/// maps.
#[must_use]
pub fn is_wildcard(ip: IpAddr) -> bool {
    ip.to_canonical().is_unspecified()
}

/// The REST listen address from `EPISCIENCE_BIND_ADDR` and `EPISCIENCE_PORT`.
///
/// The address defaults to `127.0.0.1` and must be an IP literal. Every
/// spelling of the wildcard address ([`is_wildcard`]) is REFUSED: binding every
/// interface is never the intended exposure. A deployment that must be
/// reachable from a specific non-loopback interface names that interface's
/// address. An IPv4-mapped IPv6 literal is bound as the IPv4 address it maps.
pub fn rest_bind_addr(bind: Option<&str>, port: Option<&str>) -> Result<SocketAddr, String> {
    let ip = match bind.map(str::trim).filter(|s| !s.is_empty()) {
        None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Some(raw) => raw
            .parse::<IpAddr>()
            .map_err(|_| format!("{BIND_ADDR_VAR}={raw} is not an IP address"))?
            .to_canonical(),
    };
    if is_wildcard(ip) {
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

/// Streamable-HTTP listen spec of the MCP binary (`host:port` or
/// `unix:/abs/path`); unset means stdio.
pub const LISTEN_VAR: &str = "EPISCIENCE_LISTEN";

/// Development opt-out of bearer authentication on the MCP HTTP transport.
pub const ALLOW_UNAUTHENTICATED_VAR: &str = "EPISCIENCE_ALLOW_UNAUTHENTICATED_HTTP";

/// Boot check of the MCP `EPISCIENCE_LISTEN` spec.
///
/// - `unix:/abs/path` is always accepted (filesystem permissions gate it).
/// - Otherwise the spec must be `<IP literal>:<port>` (IPv6 in brackets) or
///   `localhost:<port>`. Any other host name is refused, because what it
///   resolves to is not visible here.
/// - Every spelling of the wildcard address ([`is_wildcard`]) is refused, as
///   for the REST bind.
/// - With the development opt-out (`allow_unauthenticated`), only a loopback
///   address, `localhost` or a unix socket is accepted: an unauthenticated
///   listener never faces another interface.
pub fn mcp_listen_guard(listen: &str, allow_unauthenticated: bool) -> Result<(), String> {
    if listen.starts_with("unix:") {
        return Ok(());
    }
    let loopback = match listen.parse::<SocketAddr>() {
        Ok(addr) => {
            let ip = addr.ip().to_canonical();
            if is_wildcard(ip) {
                return Err(format!(
                    "{LISTEN_VAR}={listen} is refused: binding every interface is not \
                     allowed; use 127.0.0.1:<port>, a specific address, or unix:/abs/path"
                ));
            }
            ip.is_loopback()
        }
        Err(_) => match listen.rsplit_once(':') {
            Some(("localhost", port)) if port.parse::<u16>().is_ok() => true,
            _ => {
                return Err(format!(
                    "{LISTEN_VAR}={listen} is refused: use <IP literal>:<port>, \
                     localhost:<port> or unix:/abs/path"
                ))
            }
        },
    };
    if allow_unauthenticated && !loopback {
        return Err(format!(
            "{LISTEN_VAR}={listen} is refused with {ALLOW_UNAUTHENTICATED_VAR}: an \
             unauthenticated listener must be loopback or a unix socket"
        ));
    }
    Ok(())
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
        for wildcard in [
            "0.0.0.0",
            "::",
            "0:0:0:0:0:0:0:0",
            "::ffff:0.0.0.0",
            "0:0:0:0:0:ffff:0:0",
        ] {
            let err = rest_bind_addr(Some(wildcard), Some("8092")).unwrap_err();
            assert!(err.contains("refused"), "{wildcard}: {err}");
        }
        // An IPv4-mapped loopback is loopback, bound as the IPv4 address.
        assert_eq!(
            rest_bind_addr(Some("::ffff:127.0.0.1"), Some("8092")).unwrap(),
            "127.0.0.1:8092".parse().unwrap()
        );
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

    #[test]
    fn mcp_listen_refuses_wildcards_and_unauthenticated_non_loopback() {
        // Accepted with authentication on.
        for ok in [
            "127.0.0.1:8093",
            "[::1]:8093",
            "[::ffff:127.0.0.1]:8093",
            "localhost:8093",
            "192.0.2.10:8093",
            "unix:/run/episcience/mcp.sock",
        ] {
            assert!(mcp_listen_guard(ok, false).is_ok(), "{ok}");
        }
        // Every wildcard spelling is refused, with or without the opt-out.
        for wildcard in [
            "0.0.0.0:8093",
            "[::]:8093",
            "[::ffff:0.0.0.0]:8093",
            "[0:0:0:0:0:ffff:0:0]:8093",
        ] {
            for opt_out in [false, true] {
                let err = mcp_listen_guard(wildcard, opt_out).unwrap_err();
                assert!(err.contains("every interface"), "{wildcard}: {err}");
            }
        }
        // A host name other than localhost is refused.
        assert!(mcp_listen_guard("example.org:8093", false).is_err());
        assert!(mcp_listen_guard("localhost:port", false).is_err());
        // The opt-out is accepted only on loopback or a unix socket.
        for ok in [
            "127.0.0.1:8093",
            "[::1]:8093",
            "localhost:8093",
            "unix:/tmp/m.sock",
        ] {
            assert!(mcp_listen_guard(ok, true).is_ok(), "{ok}");
        }
        let err = mcp_listen_guard("192.0.2.10:8093", true).unwrap_err();
        assert!(err.contains(ALLOW_UNAUTHENTICATED_VAR), "{err}");
    }
}
