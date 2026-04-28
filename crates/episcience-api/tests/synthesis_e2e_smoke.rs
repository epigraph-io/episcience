//! End-to-end smoke test placeholder (Phase 5 Task 5.5).
//!
//! ## Status
//!
//! This file is a **placeholder for the manual Task 5.6 verification** —
//! the actual smoke run against a real EpiGraph instance with an ingested
//! corpus is the user's responsibility (see the Phase 5 validation plan).
//!
//! The intent of the placeholder is two-fold:
//!  1. Pin the test target name and the required-features gate so the
//!     manual run command is mechanical:
//!        ```
//!        cargo test -p episcience-api --features e2e-smoke \
//!          -- --ignored synthesis_e2e_smoke
//!        ```
//!  2. Reserve a place for the future automated smoke (when CI gains
//!     access to a long-lived EpiGraph dev instance) without forcing
//!     every CI run to provision one today.
//!
//! ## Required environment (when run manually)
//!
//! - `EPIGRAPH_API_URL`        — base URL for the upstream EpiGraph API
//! - `EPIGRAPH_SERVICE_TOKEN`  — bearer token with the `claims:read` /
//!                               `events:read` / `edges:write` scopes
//! - `DATABASE_URL`            — Postgres DSN for `epigraph_dev_synthesis`
//!                               (the synthesis side database)
//!
//! Optional:
//!
//! - `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` — if set, the smoke uses real
//!   LLM/embedder; otherwise it falls back to mocks (which makes the test
//!   degenerate — comparable to the existing
//!   `synthesis_job_handler_test::synthesis_handler_runs_all_stages_to_completion`).
//!
//! ## Why feature-gated rather than `#[ignore]`-only
//!
//! `#[ignore]` is per-test; `cargo test --ignored` would still compile and
//! link the test binary. Feature-gating with `required-features` skips the
//! whole compilation unit, which keeps default builds free of the e2e
//! deps and lets us add real-network helper crates later without bloating
//! every `cargo test` invocation.

#![cfg(feature = "e2e-smoke")]

#[tokio::test]
#[ignore = "e2e: requires real EPIGRAPH_API_URL + service token + ingested corpus (Task 5.6)"]
async fn smoke_synthesize_against_real_subgraph() {
    let api = std::env::var("EPIGRAPH_API_URL")
        .expect("EPIGRAPH_API_URL must be set for the e2e smoke (Task 5.6 manual verification)");
    let token = std::env::var("EPIGRAPH_SERVICE_TOKEN").expect(
        "EPIGRAPH_SERVICE_TOKEN must be set for the e2e smoke (Task 5.6 manual verification)",
    );
    let database_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL (epigraph_dev_synthesis DSN) must be set for the e2e smoke");

    // Surface the env so a hand-runner sees what got picked up.
    eprintln!("e2e-smoke placeholder running against api={api}");
    eprintln!(
        "  database_url=<{} chars>, token=<{} chars>",
        database_url.len(),
        token.len()
    );

    // Connect to the synthesis DB to confirm wiring before the user
    // implements the actual synthesize-and-assert body.
    let _pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("connect to synthesis DB");

    eprintln!(
        "e2e-smoke stub OK — synthesis DB reachable; \
         body of the smoke is left to the manual Task 5.6 run \
         (see docs/superpowers/plans/p5-validation.md for the manual checklist)."
    );
}
