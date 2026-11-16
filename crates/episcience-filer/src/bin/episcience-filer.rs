//! `episcience-filer`: the Experiment Filer in shadow mode (phase 0).
//!
//! Scans `EPISCIENCE_FILER_ROOT` for complete run directories, validates
//! each against the run contract and appends the outcome and the filing plan
//! it WOULD execute to the journal at `EPISCIENCE_FILER_JOURNAL`. With
//! `EPISCIENCE_FILER_POLL_SECS` it keeps scanning; without, it scans once.
//!
//! It holds no credential, opens no socket and no database connection, and
//! writes nothing but its journal. One JSON summary per scan goes to stdout.

use std::sync::atomic::AtomicBool;

use episcience_filer::config::FilerConfig;
use episcience_filer::journal::Journal;
use episcience_filer::watcher::{ScanSummary, Watcher, WatcherOptions};

fn refuse(msg: impl std::fmt::Display) -> ! {
    eprintln!("ERROR: {msg}");
    std::process::exit(2);
}

fn report(summary: &ScanSummary) {
    match serde_json::to_string(summary) {
        Ok(line) => println!("{line}"),
        Err(e) => eprintln!("WARN: scan summary not printed: {e}"),
    }
}

fn main() {
    let config = FilerConfig::from_env(|name| std::env::var_os(name)).unwrap_or_else(|e| refuse(e));
    let mut journal = Journal::open(&config.journal).unwrap_or_else(|e| refuse(e));
    if journal.torn_tail_bytes() > 0 {
        eprintln!(
            "WARN: dropped a torn journal tail of {} bytes (an append interrupted by a crash)",
            journal.torn_tail_bytes()
        );
    }
    let watcher = Watcher::new(config.root, WatcherOptions::default());
    let outcome = match config.poll {
        None => watcher
            .scan_once(&mut journal)
            .map(|summary| report(&summary)),
        Some(interval) => {
            // Never set: the loop ends when the process is stopped. An
            // interrupted append is a torn tail, dropped at the next start.
            let stop = AtomicBool::new(false);
            watcher.run(&mut journal, interval, &stop, report)
        }
    };
    if let Err(e) = outcome {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}
