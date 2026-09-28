//! Boot-time configuration checks shared by the binaries.
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

/// A process-environment variable as every boot check reads it: `None` only
/// when the variable is UNSET. A set value that is not UTF-8 is kept, lossily
/// converted (`U+FFFD` for the invalid bytes), so a refusal keyed on presence
/// refuses it and a parser of the value rejects it; `std::env::var(..).ok()`
/// would read it as unset and let it through.
#[must_use]
pub fn env_value(name: &str) -> Option<String> {
    std::env::var_os(name).map(|v| v.to_string_lossy().into_owned())
}

/// Variables whose presence makes EVERY EpiScience binary refuse to start
/// (E1h): the retired service client's credentials and token, and the
/// retired service identity. Nothing reads them any more (the service client
/// and its code are deleted; every write acts as the calling or job
/// principal), so one left in a unit environment is a stale credential that
/// writes the kernel with no principal (D-M5), or a unit that predates the
/// tenancy port. Judged on presence (even empty, even not UTF-8), before any
/// other check.
pub const RETIRED_SERVICE_VARS: [&str; 4] = [
    "EPIGRAPH_CLIENT_ID",
    "EPIGRAPH_CLIENT_SECRET",
    "EPIGRAPH_SERVICE_TOKEN",
    "EPIGRAPH_SERVICE_AGENT_ID",
];

/// Variables nothing reads any more and whose presence is harmless (a
/// switch for a deleted component, the retired client's endpoint): a
/// binary that finds one WARNS once, naming it, and carries on.
pub const RETIRED_HARMLESS_VARS: [&str; 2] = ["EPISCIENCE_INPROCESS_WORKER", "EPIGRAPH_API_URL"];

/// The boot refusal every binary runs first: `Err` naming the first of
/// [`RETIRED_SERVICE_VARS`] that is set (through `get`: the process
/// environment in production, a map in tests), and `binary`.
pub fn refuse_retired_service_vars(
    binary: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    for var in RETIRED_SERVICE_VARS {
        if get(var).is_some() {
            return Err(format!(
                "{var} is set: {binary} refuses to start with a retired service-client or \
                 service-identity variable in its environment (nothing reads it; every write \
                 acts as the calling or job principal). Remove it from the unit environment"
            ));
        }
    }
    Ok(())
}

/// The [`RETIRED_HARMLESS_VARS`] that are set, for the one-line boot warning.
#[must_use]
pub fn retired_harmless_vars_set(get: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    RETIRED_HARMLESS_VARS
        .into_iter()
        .filter(|v| get(v).is_some())
        .collect()
}

/// The DSN variable the REST and MCP servers read: the `episcience_app`
/// application login (the unit's `EnvironmentFile`; neither binary loads a
/// `.env` file).
pub const REQUEST_DATABASE_URL_VAR: &str = "DATABASE_URL";

/// Variables whose presence makes the REST and MCP servers refuse to start:
/// a privileged DSN of any kind (the kernel maintenance DSN, the EpiScience
/// migration owner's), and the DSNs of the OTHER EpiScience logins: the
/// maintenance login's (it holds the cross-owner maintenance definers: the
/// narrowing sweep and the backfill) and the worker's (it holds the queue
/// definers). A request-serving process holds its own login only, exactly as
/// the worker refuses `DATABASE_URL`.
pub const REQUEST_FORBIDDEN_VARS: [&str; 4] = [
    "MAINTENANCE_DATABASE_URL",
    "EPISCIENCE_MIGRATION_DATABASE_URL",
    "EPISCIENCE_MAINT_DATABASE_URL",
    "EPISCIENCE_WORKER_DATABASE_URL",
];

/// The request DSN, or the refusal: any of [`REQUEST_FORBIDDEN_VARS`] set
/// (even empty), or [`REQUEST_DATABASE_URL_VAR`] unset or empty. The DSN's
/// ROLE is judged after connecting (`EpiscienceDb::connect` refuses a
/// superuser, BYPASSRLS or maintenance-member session).
pub fn request_database_url(
    binary: &str,
    get: impl Fn(&str) -> Option<String>,
) -> Result<String, String> {
    for var in REQUEST_FORBIDDEN_VARS {
        if get(var).is_some() {
            return Err(format!(
                "{var} is set: {binary} refuses to start with a privileged DSN in its \
                 environment (remove it from the unit environment)"
            ));
        }
    }
    match get(REQUEST_DATABASE_URL_VAR) {
        Some(u) if !u.trim().is_empty() => Ok(u),
        _ => Err(format!(
            "{REQUEST_DATABASE_URL_VAR} must name the episcience_app login's DSN"
        )),
    }
}

/// The DSN variable `episcience-worker` reads (the `episcience_worker`
/// application login). It reads no other DSN variable.
pub const WORKER_DATABASE_URL_VAR: &str = "EPISCIENCE_WORKER_DATABASE_URL";

/// Variables whose presence makes `episcience-worker` refuse to start: a
/// privileged DSN of any kind (the kernel maintenance DSN, the EpiScience
/// migration owner's) and the DSNs of the OTHER EpiScience logins: the
/// request login's `DATABASE_URL` (in the checkout's file it is a superuser
/// DSN) and the maintenance login's (it holds the cross-owner maintenance
/// definers). Each process holds its own login only, as the request servers
/// and `episcience-maint` do. (The retired service-client variables are
/// refused by every binary: [`RETIRED_SERVICE_VARS`].)
pub const WORKER_FORBIDDEN_VARS: [&str; 4] = [
    "MAINTENANCE_DATABASE_URL",
    "EPISCIENCE_MIGRATION_DATABASE_URL",
    "EPISCIENCE_MAINT_DATABASE_URL",
    "DATABASE_URL",
];

/// The DSN variable `episcience-maint` reads (the `episcience_maint` login).
pub const MAINT_DATABASE_URL_VAR: &str = "EPISCIENCE_MAINT_DATABASE_URL";

/// Variables whose presence makes `episcience-maint` refuse to start: a
/// privileged DSN (the kernel maintenance DSN, the EpiScience migration
/// owner's) and the DSNs of the OTHER EpiScience logins: the request login's
/// `DATABASE_URL` and the worker's (it holds the queue definers).
pub const MAINT_FORBIDDEN_VARS: [&str; 4] = [
    "DATABASE_URL",
    "EPISCIENCE_MIGRATION_DATABASE_URL",
    "MAINTENANCE_DATABASE_URL",
    "EPISCIENCE_WORKER_DATABASE_URL",
];

/// The worker's DSN, or the refusal: any of [`WORKER_FORBIDDEN_VARS`] set
/// (even empty), or [`WORKER_DATABASE_URL_VAR`] unset or empty. `get` reads
/// one variable (the process environment in production, a map in tests).
pub fn worker_database_url(get: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    for var in WORKER_FORBIDDEN_VARS {
        if get(var).is_some() {
            return Err(format!(
                "{var} is set: episcience-worker refuses to start with it (remove it from the \
                 unit environment)"
            ));
        }
    }
    match get(WORKER_DATABASE_URL_VAR) {
        Some(u) if !u.trim().is_empty() => Ok(u),
        _ => Err(format!(
            "{WORKER_DATABASE_URL_VAR} must name the episcience_worker login's DSN"
        )),
    }
}

#[cfg(test)]
mod worker_config_tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    /// Each forbidden variable, even EMPTY, refuses the worker and is named in
    /// the refusal. Kills: dropping one entry, or testing for a non-empty
    /// value only.
    #[test]
    fn every_forbidden_variable_refuses_the_worker_even_when_empty() {
        for var in WORKER_FORBIDDEN_VARS {
            for value in ["x", ""] {
                let e = worker_database_url(env(&[
                    (WORKER_DATABASE_URL_VAR, "postgres://w@h/d"),
                    (var, value),
                ]))
                .expect_err(var);
                assert!(e.contains(var), "{e}");
            }
        }
        for var in [
            "MAINTENANCE_DATABASE_URL",
            "EPISCIENCE_MIGRATION_DATABASE_URL",
            "EPISCIENCE_MAINT_DATABASE_URL",
            "DATABASE_URL",
        ] {
            assert!(WORKER_FORBIDDEN_VARS.contains(&var), "{var}");
        }
    }

    /// Every DSN variable of another EpiScience login, and every privileged
    /// one, is refused by each process that does not own it: no process
    /// boots holding a second login. Kills: dropping any entry from any of
    /// the three lists (the reviewer's D3 mirror cases included).
    #[test]
    fn each_process_refuses_every_other_logins_dsn_variable() {
        const PRIVILEGED: [&str; 2] = [
            "MAINTENANCE_DATABASE_URL",
            "EPISCIENCE_MIGRATION_DATABASE_URL",
        ];
        let lists: [(&str, &str, &[&str]); 3] = [
            ("request", REQUEST_DATABASE_URL_VAR, &REQUEST_FORBIDDEN_VARS),
            ("worker", WORKER_DATABASE_URL_VAR, &WORKER_FORBIDDEN_VARS),
            ("maint", MAINT_DATABASE_URL_VAR, &MAINT_FORBIDDEN_VARS),
        ];
        for (who, own, forbidden) in lists {
            assert!(!forbidden.contains(&own), "{who} refuses its own DSN");
            for (_, other, _) in lists.iter().filter(|(_, o, _)| *o != own) {
                assert!(forbidden.contains(other), "{who} must refuse {other}");
            }
            for p in PRIVILEGED {
                assert!(forbidden.contains(&p), "{who} must refuse {p}");
            }
        }
    }

    /// Only the dedicated variable is read; unset or blank is refused.
    #[test]
    fn the_worker_reads_only_its_own_dsn_variable() {
        assert_eq!(
            worker_database_url(env(&[(WORKER_DATABASE_URL_VAR, "postgres://w@h/d")])).unwrap(),
            "postgres://w@h/d"
        );
        assert!(worker_database_url(env(&[])).is_err());
        assert!(worker_database_url(env(&[(WORKER_DATABASE_URL_VAR, "  ")])).is_err());
    }

    /// T-H1 (unit half): each retired service-client or service-identity
    /// variable, even EMPTY, is refused and named with the binary; both the
    /// brief's variables are on the list; none set passes. Kills: dropping an
    /// entry (a stale credential would boot silently), or testing for a
    /// non-empty value only.
    #[test]
    fn every_retired_service_variable_refuses_boot_even_when_empty() {
        for var in ["EPIGRAPH_CLIENT_ID", "EPIGRAPH_SERVICE_AGENT_ID"] {
            assert!(RETIRED_SERVICE_VARS.contains(&var), "{var}");
        }
        for var in RETIRED_SERVICE_VARS {
            for value in ["x", ""] {
                let e = refuse_retired_service_vars("episcience-x", env(&[(var, value)]))
                    .expect_err(var);
                assert!(e.contains(var) && e.contains("episcience-x"), "{e}");
            }
        }
        assert_eq!(refuse_retired_service_vars("x", env(&[])), Ok(()));
        // The harmless leftovers are warned about, never refused.
        assert_eq!(
            refuse_retired_service_vars("x", env(&[("EPISCIENCE_INPROCESS_WORKER", "1")])),
            Ok(())
        );
        assert_eq!(
            retired_harmless_vars_set(env(&[("EPISCIENCE_INPROCESS_WORKER", "0")])),
            vec!["EPISCIENCE_INPROCESS_WORKER"]
        );
        for var in RETIRED_HARMLESS_VARS {
            assert!(!RETIRED_SERVICE_VARS.contains(&var), "{var}");
        }
    }

    /// Each privileged or foreign-login DSN variable, even EMPTY, refuses the
    /// request servers and is named; `DATABASE_URL` is required. Kills:
    /// dropping the `MAINTENANCE_DATABASE_URL` refusal (brief E1g requirement
    /// 4), dropping the maintenance or worker login's variable (a request
    /// process would boot holding the cross-owner maintenance definers or the
    /// queue definers), or testing for a non-empty value only.
    #[test]
    fn request_servers_refuse_a_privileged_dsn_variable() {
        for var in REQUEST_FORBIDDEN_VARS {
            for value in ["x", ""] {
                let e = request_database_url(
                    "episcience-server",
                    env(&[(REQUEST_DATABASE_URL_VAR, "postgres://a@h/d"), (var, value)]),
                )
                .expect_err(var);
                assert!(e.contains(var) && e.contains("episcience-server"), "{e}");
            }
        }
        for var in [
            "MAINTENANCE_DATABASE_URL",
            "EPISCIENCE_MIGRATION_DATABASE_URL",
            WORKER_DATABASE_URL_VAR,
            "EPISCIENCE_MAINT_DATABASE_URL",
        ] {
            assert!(REQUEST_FORBIDDEN_VARS.contains(&var), "{var}");
        }
        assert_eq!(
            request_database_url("x", env(&[(REQUEST_DATABASE_URL_VAR, "postgres://a@h/d")]))
                .unwrap(),
            "postgres://a@h/d"
        );
        assert!(request_database_url("x", env(&[])).is_err());
        assert!(request_database_url("x", env(&[(REQUEST_DATABASE_URL_VAR, " ")])).is_err());
    }
}
