//! The filer's kernel credential source is named in the deploy docs and is
//! not a retired service variable (backlog 245bbdd1).
//!
//! `docs/deploy.md` and `docs/experiment-filer.md` name
//! `EPISCIENCE_FILER_CREDENTIALS_FILE` as the one place the phase-1 writer
//! reads its kernel service-client credential from. Every EpiScience binary
//! refuses the retired service variables at boot, so the documented name
//! must not be one of them (a filer configured as documented would never
//! start), and phase 0, which holds no credential, must start with it set.

use std::ffi::OsString;
use std::path::Path;

use episcience_filer::config::{FilerConfig, ENV_JOURNAL, ENV_ROOT, RETIRED_SERVICE_VARS};

const CREDENTIALS_VAR: &str = "EPISCIENCE_FILER_CREDENTIALS_FILE";

fn doc(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn deploy_docs_name_the_filer_credential_source() {
    let deploy = doc("deploy.md");
    assert!(
        deploy.contains(CREDENTIALS_VAR),
        "docs/deploy.md must name the filer's credential source {CREDENTIALS_VAR}"
    );
    let filer = doc("experiment-filer.md");
    assert!(
        filer.contains(CREDENTIALS_VAR),
        "docs/experiment-filer.md must point the phase-1 writer at {CREDENTIALS_VAR}"
    );
    assert!(
        !filer.contains("needs c4cad28e"),
        "the service identity path is specified; the filer docs must not list it as open"
    );
}

#[test]
fn the_credential_source_is_not_a_retired_variable_and_phase_0_starts_with_it() {
    assert!(!RETIRED_SERVICE_VARS.contains(&CREDENTIALS_VAR));
    let cfg = FilerConfig::from_env(|name| match name {
        ENV_ROOT => Some(OsString::from("/srv/campaigns")),
        ENV_JOURNAL => Some(OsString::from("/var/lib/episcience-filer/outbox.jsonl")),
        CREDENTIALS_VAR => Some(OsString::from("/etc/episcience/filer.credentials")),
        _ => None,
    });
    assert!(cfg.is_ok(), "{cfg:?}");
}
