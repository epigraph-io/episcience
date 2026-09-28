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

/// The shared token secret, or an error when it is unset or empty, or when
/// the kernel's own `epigraph_auth::assert_production_secret` refuses it
/// (shorter than `epigraph_auth::MIN_SECRET_LEN` bytes, or the committed
/// development literal).
///
/// There is deliberately NO fallback and NO opt-out: a process without a real
/// secret must not start, rather than verify tokens against a guessable or
/// publicly known key. The kernel MCP server applies the same function to the
/// same shared secret with no opt-out.
pub fn require_jwt_secret(value: Option<String>) -> Result<Vec<u8>, String> {
    let secret = match value {
        Some(s) if !s.is_empty() => s.into_bytes(),
        _ => {
            return Err(format!(
                "{JWT_SECRET_VAR} must be set: EpiScience verifies kernel-minted access tokens \
                 with it and has no development fallback"
            ))
        }
    };
    epigraph_auth::assert_production_secret(&secret)
        .map_err(|e| format!("{JWT_SECRET_VAR} is refused: {e}"))?;
    Ok(secret)
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
            .map_err(|_| format!("{BIND_ADDR_VAR}={raw} is not an IP address"))?,
    };
    // `is_wildcard` is the one place the wildcard judgement canonicalises.
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
    Ok(SocketAddr::new(ip.to_canonical(), port))
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
///
///   This is LOOSER than the kernel's `epigraph-mcp`, which allows its
///   unauthenticated mode on a unix socket only and refuses loopback TCP
///   (a browser page can reach loopback TCP through DNS rebinding). It is
///   accepted here because the opt-out attaches no caller: such a page could
///   only initialize and list tools, never call one.
pub fn mcp_listen_guard(listen: &str, allow_unauthenticated: bool) -> Result<(), String> {
    if listen.starts_with("unix:") {
        return Ok(());
    }
    let loopback = match listen.parse::<SocketAddr>() {
        Ok(addr) => {
            if is_wildcard(addr.ip()) {
                return Err(format!(
                    "{LISTEN_VAR}={listen} is refused: binding every interface is not \
                     allowed; use 127.0.0.1:<port>, a specific address, or unix:/abs/path"
                ));
            }
            addr.ip().to_canonical().is_loopback()
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
    fn secret_is_required_strong_and_never_defaulted() {
        assert!(require_jwt_secret(None).is_err());
        assert!(require_jwt_secret(Some(String::new())).is_err());
        let short = "x".repeat(epigraph_auth::MIN_SECRET_LEN - 1);
        assert!(require_jwt_secret(Some(short))
            .unwrap_err()
            .contains("bytes"));
        let dev = String::from_utf8(epigraph_auth::DEV_JWT_SECRET.to_vec()).unwrap();
        assert!(require_jwt_secret(Some(dev))
            .unwrap_err()
            .contains("committed dev literal"));
        let ok = "k".repeat(epigraph_auth::MIN_SECRET_LEN);
        assert_eq!(
            require_jwt_secret(Some(ok.clone())).unwrap(),
            ok.into_bytes()
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
