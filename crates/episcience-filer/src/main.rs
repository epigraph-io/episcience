//! `episcience-filer`: phase 0 (shadow mode). Writes nothing to EpiGraph or
//! EpiScience; prints the filing plan it would execute.
//!
//! ```text
//! episcience-filer plan   --root <dir>
//! episcience-filer replay --journal <file>
//! episcience-filer watch  --root <dir> --journal <file> [--interval-secs <n>] [--once]
//! ```
//!
//! `plan` prints the full plan for the root's current state. `replay` prints
//! the plan a journal holds, normalized the same way. `watch` appends each
//! pass's new records to the journal and prints them as they are journaled.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use episcience_filer::journal::Journal;
use episcience_filer::plan::{normalize, render};
use episcience_filer::watch::{plan_for_root, watch_once};

const USAGE: &str = "usage:
  episcience-filer plan   --root <dir>
  episcience-filer replay --journal <file>
  episcience-filer watch  --root <dir> --journal <file> [--interval-secs <n>] [--once]";

#[derive(Default)]
struct Args {
    root: Option<PathBuf>,
    journal: Option<PathBuf>,
    interval_secs: u64,
    once: bool,
}

fn parse(rest: &[String]) -> Result<Args, String> {
    let mut args = Args {
        interval_secs: 30,
        ..Args::default()
    };
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--root" => args.root = Some(PathBuf::from(value()?)),
            "--journal" => args.journal = Some(PathBuf::from(value()?)),
            "--interval-secs" => {
                args.interval_secs = value()?
                    .parse()
                    .map_err(|_| "--interval-secs must be a whole number".to_string())?
            }
            "--once" => args.once = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

fn emit(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|_| out.flush())
        .map_err(|e| format!("stdout: {e}"))
}

fn run(argv: &[String]) -> Result<(), String> {
    let (cmd, rest) = argv.split_first().ok_or_else(|| USAGE.to_string())?;
    let args = parse(rest)?;
    let need = |v: Option<PathBuf>, flag: &str| v.ok_or(format!("{cmd} needs {flag}\n{USAGE}"));
    match cmd.as_str() {
        "plan" => {
            let root = need(args.root, "--root")?;
            let plan = plan_for_root(&root).map_err(|e| format!("scan {}: {e}", root.display()))?;
            emit(&render(&plan))
        }
        "replay" => {
            let journal =
                Journal::open(&need(args.journal, "--journal")?).map_err(|e| e.to_string())?;
            emit(&render(&normalize(journal.records().to_vec())))
        }
        "watch" => {
            let root = need(args.root, "--root")?;
            let mut journal =
                Journal::open(&need(args.journal, "--journal")?).map_err(|e| e.to_string())?;
            loop {
                let fresh = watch_once(&root, &mut journal).map_err(|e| e.to_string())?;
                emit(&render(&fresh))?;
                if args.once {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_secs(args.interval_secs));
            }
        }
        _ => Err(USAGE.to_string()),
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match run(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("episcience-filer: {msg}");
            ExitCode::FAILURE
        }
    }
}
