//! Crate-local abstraction over the EpiGraph edges service.
//!
//! Stage 6 (publish) needs to POST edges back to EpiGraph, but the concrete
//! HTTP client (`episcience_api::clients::epigraph_edges::EpigraphEdgesClient`)
//! lives in `episcience-api`, which already depends on `episcience-db`. To
//! avoid a circular dependency, the pipeline takes a `&dyn EdgeWriter` here
//! and the api crate provides the concrete `impl EdgeWriter for
//! EpigraphEdgesClient` adapter.
//!
//! The trait is `Send + Sync` so trait objects survive `.await` points in
//! `stage6_write_edges`.
//!
//! `EdgeWriterError` is a small, transport-agnostic enum mirroring the same
//! variant structure as `episcience_api::errors::ApiError`. The api-side
//! adapter folds `ApiError` into `EdgeWriterError` via a local `From` impl;
//! the pipeline folds this into [`SynthesisError::EdgeWrite`] at the boundary.
//!
//! `EdgeRequest` is moved here (rather than re-exporting `episcience_api`'s
//! version) because the pipeline is the canonical caller — keeping the type
//! co-located with the trait avoids forcing every db-side consumer to depend
//! on the api crate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// A request to create a single PROV-O-style edge in EpiGraph.
///
/// Mirrors the shape POSTed to `/edges` by the production
/// `EpigraphEdgesClient`. Field names match the JSON wire format so the
/// api-side adapter can pass this struct through unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EdgeRequest {
    pub source_type: String,
    pub source_id: Uuid,
    pub target_type: String,
    pub target_id: Uuid,
    pub relationship: String,
}

/// Transport-agnostic error returned by [`EdgeWriter::create_edge`].
///
/// Variants intentionally parallel the relevant subset of
/// `episcience_api::errors::ApiError` so the api-side adapter is a 1:1 map.
#[derive(Debug, Error)]
pub enum EdgeWriterError {
    #[error("edge service unavailable: {0}")]
    ServiceUnavailable(String),
    #[error("edge service rejected request as invalid: {0}")]
    Validation(String),
    #[error("edge service returned an unexpected error: {0}")]
    Internal(String),
}

/// Abstraction over the EpiGraph edges service.
///
/// Implementors POST a single edge per call and return the new edge's UUID on
/// success. The Stage 6 pipeline calls this once per planned provenance edge.
///
/// `Send + Sync` are required so `&dyn EdgeWriter` can cross `.await` points
/// inside `stage6_write_edges`.
#[async_trait]
pub trait EdgeWriter: Send + Sync {
    async fn create_edge(&self, req: EdgeRequest) -> Result<Uuid, EdgeWriterError>;
}
