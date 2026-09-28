use thiserror::Error;

#[derive(Error, Debug)]
pub enum DbError {
    #[error("Database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("Not found: {entity} {id}")]
    NotFound { entity: String, id: String },

    #[error("Constraint violation: {0}")]
    Constraint(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    /// A write the tenancy rules refuse, detected by the application before
    /// the database's row guard would (the same rule and the same words as
    /// that guard, so the caller sees one answer either way).
    #[error("refused by the tenancy guard: {0}")]
    TenancyRefused(String),
}
