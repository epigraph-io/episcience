use std::path::{Path, PathBuf};

use thiserror::Error;

/// A failure of the filer itself (its journal, its root, its configuration).
///
/// A run directory that fails validation is NOT an error: it is a
/// [`crate::validate::Rejection`], journaled like any other outcome.
#[derive(Error, Debug)]
pub enum FilerError {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Journal {path} is corrupt at line {line}: {detail}")]
    JournalCorrupt {
        path: PathBuf,
        line: usize,
        detail: String,
    },

    #[error("Journal serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("Configuration error: {0}")]
    Config(String),
}

impl FilerError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}
