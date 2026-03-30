use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum ElnError {
    #[error("Sample {id} not found")]
    SampleNotFound { id: Uuid },

    #[error("Invalid sample status transition from {from} to {to}")]
    InvalidStatusTransition { from: String, to: String },

    #[error("Protocol {id} not found")]
    ProtocolNotFound { id: Uuid },

    #[error("Validation error: {field} — {reason}")]
    Validation { field: String, reason: String },

    #[error("Database error: {0}")]
    Database(String),
}
