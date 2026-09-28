//! `episcience-migrate` -- EpiScience's own schema ledger.
//!
//! ```text
//! episcience-migrate run             apply pending EpiScience migrations
//! episcience-migrate status          embedded versions, recorded versions
//! episcience-migrate adopt-baseline  record 5032 on a legacy database whose
//!                                    live tables match the committed fingerprint
//! episcience-migrate verify          ledger complete and consistent, kernel
//!                                    ledger isolated, tenancy contract v1
//!                                    holds (exit code = the deploy guard)
//! episcience-migrate fingerprint-sql print the exact fingerprint query that
//!                                    adopt-baseline runs, as one self-contained
//!                                    SELECT (no database, no environment)
//! ```
//!
//! Every subcommand except `fingerprint-sql` reads ONLY
//! `EPISCIENCE_MIGRATION_DATABASE_URL`, and refuses to start while
//! `DATABASE_URL` is set: the runtime DSN and the migration DSN are different
//! credentials, and a migrator that silently fell back to the runtime DSN is
//! how a runtime credential ends up needing DDL rights. No `.env` file is read.
//!
//! Every connection runs with `search_path = episcience_meta`, so sqlx's
//! `_sqlx_migrations` ledger is `episcience_meta._sqlx_migrations` and the
//! kernel's `public._sqlx_migrations` is never written (the kernel's migrator
//! refuses a database holding versions it does not embed). DSNs are never
//! printed.

use episcience_db::ledger::{self, AdoptOutcome, MIGRATION_URL_VAR};

const USAGE: &str =
    "usage: episcience-migrate <run|status|adopt-baseline|verify|fingerprint-sql>\n\
    reads EPISCIENCE_MIGRATION_DATABASE_URL; refuses while DATABASE_URL is set";

/// Resolve the migration DSN from an environment lookup.
fn resolve_url(get: impl Fn(&str) -> Option<String>) -> Result<String, String> {
    if get("DATABASE_URL").is_some() {
        return Err(format!(
            "refusing: DATABASE_URL is set. episcience-migrate reads only {MIGRATION_URL_VAR}; \
             unset DATABASE_URL (the runtime DSN) and run it with the migration DSN"
        ));
    }
    match get(MIGRATION_URL_VAR) {
        Some(u) if !u.trim().is_empty() => Ok(u),
        _ => Err(format!("{MIGRATION_URL_VAR} is not set")),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run,
    Status,
    AdoptBaseline,
    Verify,
    FingerprintSql,
}

fn parse_command(args: &[String]) -> Result<Command, String> {
    match args {
        [c] if c == "run" => Ok(Command::Run),
        [c] if c == "status" => Ok(Command::Status),
        [c] if c == "adopt-baseline" => Ok(Command::AdoptBaseline),
        [c] if c == "verify" => Ok(Command::Verify),
        [c] if c == "fingerprint-sql" => Ok(Command::FingerprintSql),
        _ => Err(USAGE.to_string()),
    }
}

#[tokio::main]
async fn main() {
    std::process::exit(real_main().await);
}

async fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse_command(&args) {
        Ok(c) => c,
        Err(u) => {
            eprintln!("{u}");
            return 2;
        }
    };
    if cmd == Command::FingerprintSql {
        // Needs no database: the pre-deploy comparison runs this text on a
        // read-only session and diffs it against the committed fingerprint.
        println!("{}", ledger::fingerprint_sql_inline().trim());
        return 0;
    }
    let url = match resolve_url(|k| std::env::var(k).ok()) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("episcience-migrate: {e}");
            return 2;
        }
    };
    let mut conn = match ledger::connect(&url).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("episcience-migrate: connect: {e}");
            return 1;
        }
    };

    let result = match cmd {
        Command::Run => ledger::run(&mut conn).await.map(|()| {
            println!("episcience-migrate: run complete");
        }),
        Command::AdoptBaseline => ledger::adopt_baseline(&mut conn).await.map(|o| match o {
            AdoptOutcome::Adopted => println!(
                "episcience-migrate: live tables match the 5032 fingerprint; 5032 recorded in \
                 episcience_meta._sqlx_migrations"
            ),
            AdoptOutcome::AlreadyRecorded => {
                println!("episcience-migrate: 5032 already recorded; nothing to do")
            }
        }),
        Command::Status => status(&mut conn).await,
        Command::FingerprintSql => unreachable!("handled before connecting"),
        Command::Verify => ledger::verify(&mut conn).await.map(|()| {
            println!(
                "episcience-migrate: verify OK (ledger complete and consistent; kernel ledger \
                 isolated; tenancy contract v1 holds)"
            );
        }),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("episcience-migrate: {e}");
            1
        }
    }
}

async fn status(conn: &mut sqlx::PgConnection) -> Result<(), ledger::LedgerError> {
    let embedded: Vec<i64> = ledger::MIGRATOR.iter().map(|m| m.version).collect();
    let recorded = ledger::ledger_rows(conn).await?;
    let foreign = ledger::foreign_versions_in_kernel_ledger(conn).await?;
    println!("embedded:  {embedded:?}");
    println!("recorded:  {recorded:?}");
    let pending: Vec<i64> = embedded
        .iter()
        .copied()
        .filter(|v| !recorded.iter().any(|(r, ok)| r == v && *ok))
        .collect();
    println!("pending:   {pending:?}");
    println!("kernel ledger versions >= 5000 (must be empty): {foreign:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    /// Kills: a fallback to DATABASE_URL, or honouring the migration DSN while
    /// the runtime DSN is also loaded.
    #[test]
    fn refuses_database_url_even_alongside_the_migration_url() {
        let only_runtime = resolve_url(env(&[("DATABASE_URL", "postgres://a/b")]));
        assert!(only_runtime.unwrap_err().contains("DATABASE_URL is set"));
        let both = resolve_url(env(&[
            ("DATABASE_URL", "postgres://a/b"),
            (MIGRATION_URL_VAR, "postgres://c/d"),
        ]));
        assert!(both.is_err());
        assert_eq!(
            resolve_url(env(&[(MIGRATION_URL_VAR, "postgres://c/d")])).unwrap(),
            "postgres://c/d"
        );
        assert!(resolve_url(env(&[])).is_err());
    }

    #[test]
    fn only_the_five_subcommands_parse() {
        let a = |s: &str| vec![s.to_string()];
        assert_eq!(parse_command(&a("run")), Ok(Command::Run));
        assert_eq!(parse_command(&a("status")), Ok(Command::Status));
        assert_eq!(
            parse_command(&a("adopt-baseline")),
            Ok(Command::AdoptBaseline)
        );
        assert_eq!(parse_command(&a("verify")), Ok(Command::Verify));
        assert_eq!(
            parse_command(&a("fingerprint-sql")),
            Ok(Command::FingerprintSql)
        );
        assert!(parse_command(&a("migrate")).is_err());
        assert!(parse_command(&[]).is_err());
        assert!(parse_command(&["run".into(), "extra".into()]).is_err());
    }
}
