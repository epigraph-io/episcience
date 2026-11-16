//! The filer binary's configuration, read from the environment by name.
//!
//! Phase 0 reads three variables and holds no credential:
//!
//! | Variable | Meaning |
//! |---|---|
//! | `EPISCIENCE_FILER_ROOT` | Required. The directory scanned for run directories. Read only. |
//! | `EPISCIENCE_FILER_JOURNAL` | Required. The outbox journal file (keep it outside the root, on durable storage). |
//! | `EPISCIENCE_FILER_POLL_SECS` | Optional. Seconds between scans; unset: scan once and exit. |
//!
//! Like every EpiScience binary (`docs/deploy.md`), it refuses to start when
//! a retired service-client or service-identity variable is set, even empty.
//! The list mirrors `episcience_api::config::RETIRED_SERVICE_VARS`; it is
//! repeated here because this crate must not depend on `episcience-api`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use crate::errors::FilerError;

pub const ENV_ROOT: &str = "EPISCIENCE_FILER_ROOT";
pub const ENV_JOURNAL: &str = "EPISCIENCE_FILER_JOURNAL";
pub const ENV_POLL_SECS: &str = "EPISCIENCE_FILER_POLL_SECS";

/// Refused when set (see the module docs).
pub const RETIRED_SERVICE_VARS: [&str; 4] = [
    "EPIGRAPH_CLIENT_ID",
    "EPIGRAPH_CLIENT_SECRET",
    "EPIGRAPH_SERVICE_TOKEN",
    "EPIGRAPH_SERVICE_AGENT_ID",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilerConfig {
    pub root: PathBuf,
    pub journal: PathBuf,
    /// `None`: one scan, then exit.
    pub poll: Option<Duration>,
}

impl FilerConfig {
    /// Build the configuration from `get` (`std::env::var_os` in the
    /// binary). The refusals come first, before anything else is read.
    pub fn from_env(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, FilerError> {
        for var in RETIRED_SERVICE_VARS {
            if get(var).is_some() {
                return Err(FilerError::Config(format!(
                    "{var} is set: episcience-filer refuses to start with a retired \
                     service-client or service-identity variable in its environment \
                     (phase 0 holds no credential). Remove it from the unit environment"
                )));
            }
        }
        let required = |var: &str| {
            get(var)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| FilerError::Config(format!("{var} is required")))
        };
        let root = PathBuf::from(required(ENV_ROOT)?);
        let journal = PathBuf::from(required(ENV_JOURNAL)?);
        let poll = match get(ENV_POLL_SECS).filter(|v| !v.is_empty()) {
            None => None,
            Some(raw) => {
                let secs = raw
                    .to_str()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .filter(|secs| *secs > 0)
                    .ok_or_else(|| {
                        FilerError::Config(format!("{ENV_POLL_SECS} must be a whole number > 0"))
                    })?;
                Some(Duration::from_secs(secs))
            }
        };
        Ok(Self {
            root,
            journal,
            poll,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn reads_root_journal_and_poll() {
        let config = FilerConfig::from_env(env(&[
            (ENV_ROOT, "/data/runs"),
            (ENV_JOURNAL, "/var/lib/filer/outbox.jsonl"),
            (ENV_POLL_SECS, "30"),
        ]))
        .unwrap();
        assert_eq!(config.root, PathBuf::from("/data/runs"));
        assert_eq!(config.poll, Some(Duration::from_secs(30)));

        let once = FilerConfig::from_env(env(&[(ENV_ROOT, "/r"), (ENV_JOURNAL, "/j")])).unwrap();
        assert_eq!(once.poll, None);
    }

    #[test]
    fn refuses_a_retired_service_variable_even_when_empty() {
        for var in RETIRED_SERVICE_VARS {
            let err =
                FilerConfig::from_env(env(&[(ENV_ROOT, "/r"), (ENV_JOURNAL, "/j"), (var, "")]))
                    .unwrap_err();
            assert!(err.to_string().contains(var), "{err}");
        }
    }

    #[test]
    fn requires_root_and_journal_and_a_positive_poll() {
        assert!(FilerConfig::from_env(env(&[(ENV_JOURNAL, "/j")])).is_err());
        assert!(FilerConfig::from_env(env(&[(ENV_ROOT, "/r")])).is_err());
        assert!(FilerConfig::from_env(env(&[
            (ENV_ROOT, "/r"),
            (ENV_JOURNAL, "/j"),
            (ENV_POLL_SECS, "0")
        ]))
        .is_err());
    }
}
