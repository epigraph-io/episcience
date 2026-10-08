//! `wiki_generate_article` MCP tool — generate (or regenerate) the wiki
//! article for one theme (plan 2026-10-08-wiki-phase-b-articles.md, Task 4).
//!
//! An article IS a synthesis: `skill_name = 'wiki_article'`, owned by a group
//! (`visibility = 'group'`), keyed by the theme's clustering provenance
//! (`syntheses.wiki_key`, never the theme UUID) and seeded from the theme's
//! readable members (`syntheses.seed_theme_id` on the row,
//! `seed_theme_id` in the job payload: Stage 1's theme-anchored seed).
//!
//! Every read that decides the write, and the write, run on ONE stamped
//! transaction of the caller, through the same enqueue and poll path as
//! `synthesize` ([`crate::mcp::synthesize::enqueue`]). Refusals write nothing:
//! an unknown theme, a theme with fewer than
//! [`MIN_READABLE_MEMBERS`] members the CALLER can read, a theme without
//! clustering provenance, and an owner group the caller may not write.

use rmcp::model::{CallToolResult, Content};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use episcience_core::synthesis::Visibility;
use episcience_core::wiki::{article_query, WikiKey, MIN_READABLE_MEMBERS, WIKI_SKILL_NAME};
use episcience_db::{KernelClaimRepository, SynthesisRepository};

use crate::auth::tenancy::root_ownership;
use crate::mcp::errors::{from_api, internal_error, invalid_request, McpError};
use crate::mcp::synthesize::{enqueue, poll_until_terminal, EnqueueSpec};
use crate::mcp::{from_refusal, EpiscienceServer};
use crate::middleware::AuthContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WikiGenerateArticleArgs {
    /// The theme (`claim_themes.id`) to write the article about.
    #[schemars(description = "Theme id (claim_themes.id) the article is about")]
    pub theme_id: Uuid,

    /// The owning group (must be a group the caller may write). Default: the
    /// caller's own default group.
    #[schemars(
        description = "Optional owner group id (a group the caller may write); default: the caller's own group"
    )]
    #[serde(default)]
    pub owner_group_id: Option<Uuid>,

    /// If true, poll until the synthesis reaches a terminal state and return
    /// the narrative. Default `false` (returns immediately with
    /// `status='queued'`).
    #[schemars(description = "Block until terminal state. Default: false (returns 'queued').")]
    #[serde(default)]
    pub wait_for_completion: bool,

    /// Polling timeout in seconds. Clamped to 600 s. Only consulted when
    /// `wait_for_completion=true`.
    #[schemars(description = "Polling timeout (s) when wait_for_completion=true. Clamp: 600.")]
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    600
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WikiGenerateArticleResult {
    pub synthesis_id: Uuid,
    /// The page key (`WikiKey::as_slug`).
    pub wiki_key: String,
    pub status: String,
    /// Populated only when `wait_for_completion=true` and the row reaches
    /// `status='complete'`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
}

/// Free-function delegate of the `#[tool]` method on [`EpiscienceServer`].
pub async fn handle(
    server: &EpiscienceServer,
    auth: &AuthContext,
    viewer: &epigraph_db::Viewer,
    args: WikiGenerateArticleArgs,
) -> Result<CallToolResult, McpError> {
    let mut tx = server.db.write_as(viewer).await.map_err(from_refusal)?;

    // The theme and the members THIS caller can read (the job seeds as the
    // caller, so that is the article's whole input).
    let theme = KernelClaimRepository::theme_for_wiki_as(&mut *tx, viewer, args.theme_id)
        .await
        .map_err(|e| internal_error(format!("read theme: {e}")))?
        .ok_or_else(|| invalid_request(format!("theme {} not found", args.theme_id)))?;
    if theme.readable_members < MIN_READABLE_MEMBERS {
        return Err(invalid_request(format!(
            "theme has {} readable members; a wiki article needs at least {MIN_READABLE_MEMBERS}",
            theme.readable_members
        )));
    }
    let key = WikiKey::from_properties(&theme.properties).ok_or_else(|| {
        invalid_request("theme has no cluster provenance (cluster_run_id, cluster_id)")
    })?;
    let wiki_key = key.as_slug();

    // A root synthesis with requested visibility `group`, exactly as
    // `synthesize` decides it.
    let owner = root_ownership(&mut tx, viewer, args.owner_group_id, Visibility::Group)
        .await
        .map_err(from_api)?;

    let query = article_query(&theme.label, &theme.description);
    let id = enqueue(
        &mut tx,
        server,
        auth,
        EnqueueSpec {
            query: &query,
            owner,
            skill_name: WIKI_SKILL_NAME,
            parent_synthesis_id: None,
            prereq_synthesis_ids: &[],
            traversal_config: None,
            seed_theme_id: Some(args.theme_id),
        },
    )
    .await?;
    // Same transaction: the row never exists without its page key.
    SynthesisRepository::set_wiki_seed_tx(&mut *tx, id, args.theme_id, &wiki_key)
        .await
        .map_err(|e| internal_error(format!("set wiki key: {e}")))?;
    tx.commit()
        .await
        .map_err(|e| internal_error(format!("tx commit: {e}")))?;

    let mut result = WikiGenerateArticleResult {
        synthesis_id: id,
        wiki_key,
        status: "queued".to_string(),
        narrative: None,
    };
    if args.wait_for_completion {
        (result.status, result.narrative) =
            poll_until_terminal(server, viewer, id, args.timeout_seconds).await;
    }
    let body = serde_json::to_string_pretty(&result).map_err(internal_error)?;
    Ok(CallToolResult::success(vec![Content::text(body)]))
}
