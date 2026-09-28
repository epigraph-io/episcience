//! T-G1: the test harness refuses the production port and non-`_test`
//! database names, with no override. No database is contacted.
mod support;
use support::{check_test_db_name, check_test_url};

/// Kills: removing the port check in `check_test_url`.
#[test]
fn refuses_port_5432() {
    let e = check_test_url("postgres://u:p@127.0.0.1:5432/x_test").unwrap_err();
    assert!(e.contains("5432"), "{e}");
}

/// A URL with no port means 5432 to libpq; the check reads the port after
/// defaulting. Only meaningful when PGPORT does not override the default.
#[test]
fn refuses_a_url_that_defaults_to_5432() {
    if std::env::var_os("PGPORT").is_some() {
        return;
    }
    assert!(check_test_url("postgres://u:p@127.0.0.1/x_test").is_err());
}

/// Kills: removing the `_test` suffix check.
#[test]
fn refuses_a_database_name_without_the_test_suffix() {
    let e = check_test_url("postgres://u:p@127.0.0.1:5433/episcience_dev").unwrap_err();
    assert!(e.contains("_test"), "{e}");
    assert!(check_test_url("postgres://u:p@127.0.0.1:5433/x_test_old").is_err());
}

/// Kills: removing the 63-byte check (Postgres would truncate the name and
/// could cut the suffix off).
#[test]
fn refuses_names_postgres_would_truncate() {
    let long = format!("{}_test", "a".repeat(60));
    assert!(long.len() > 63);
    assert!(check_test_db_name(&long).is_err());
}

#[test]
fn accepts_a_test_database_on_another_port() {
    assert!(check_test_url("postgres://u:p@127.0.0.1:5433/episcience_e1_x_test").is_ok());
}
