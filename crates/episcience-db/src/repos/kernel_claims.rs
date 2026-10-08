//! Reads of KERNEL `claims` rows that EpiScience issues itself.
//!
//! The process still connects as a role that bypasses row-level security, so
//! the kernel's visibility rules do not apply by themselves. Every statement
//! here therefore carries the kernel's own `/* {VISIBILITY:c} */` marker and is
//! rendered with `epigraph_db::Viewer::splice`: a caller reads exactly the
//! claims the kernel would show it (public, or owned by one of its groups).
//! A claim the viewer cannot read is indistinguishable from an absent one.

use epigraph_db::Viewer;
use uuid::Uuid;

use crate::errors::DbError;

pub struct KernelClaimRepository;

/// A kernel theme as the source of a wiki article
/// ([`KernelClaimRepository::theme_for_wiki_as`]).
#[derive(Debug, Clone)]
pub struct WikiThemeSource {
    pub label: String,
    pub description: String,
    /// The theme's clustering provenance (`claim_themes.properties`), the
    /// input of `episcience_core::wiki::WikiKey::from_properties`.
    pub properties: serde_json::Value,
    /// Current PUBLIC members (the viewer can read every one): the set the
    /// production worker can seed a wiki article from until KE-1 (see
    /// [`KernelClaimRepository::theme_for_wiki_as`]).
    pub public_members: i64,
}

impl KernelClaimRepository {
    /// The content of claim `id` if `viewer` can read it; `None` when the
    /// claim is absent OR invisible to `viewer`.
    pub async fn content_as<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        viewer: &Viewer,
        id: Uuid,
    ) -> Result<Option<String>, DbError> {
        let sql = viewer.splice(
            "SELECT c.content FROM claims c WHERE c.id = $1 /* {VISIBILITY:c} */",
            2,
        );
        let mut q = sqlx::query_scalar::<_, String>(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_optional(executor).await?)
    }

    /// Claim `id`'s content and ownership pair `(content, visibility,
    /// owner_group_id)` if `viewer` can read it; `None` when absent OR
    /// invisible.
    pub async fn content_and_pair_as<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        viewer: &Viewer,
        id: Uuid,
    ) -> Result<Option<(String, String, Uuid)>, DbError> {
        let sql = viewer.splice(
            "SELECT c.content, c.visibility::text, c.owner_group_id FROM claims c \
              WHERE c.id = $1 /* {VISIBILITY:c} */",
            2,
        );
        let mut q = sqlx::query_as::<_, (String, String, Uuid)>(&sql).bind(id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_optional(executor).await?)
    }

    /// Theme `theme_id` as a wiki article source: its label, description,
    /// clustering provenance (`properties`) and the number of its CURRENT
    /// PUBLIC member claims. `None` when no such theme exists.
    ///
    /// Public only, because that is all the production worker can seed. The
    /// worker reads a theme's members (`SynthesisPipeline::stage1_seed_theme`
    /// in `crate::synthesis::pipeline`) on its unstamped `ENGINE_POOL`, and
    /// row security on an unstamped session returns public claims only
    /// (`V1-engine-takes-pool`, until KE-1; "Public-only seeding" in
    /// `docs/tenancy-contract.md`). Counting members the article's owner
    /// group may cite (public, or owned by that group) would admit a theme
    /// whose members are mostly or all group-private, and the queued job
    /// would then be seeded from its public part alone, or fail on an empty
    /// seed after its row was written.
    ///
    /// KE-1: once the engine reads as the stamped viewer, widen the count
    /// back to what the seed filter keeps for a group synthesis, i.e. add an
    /// `owner_group: Uuid` parameter and use
    /// `(c.visibility::text = 'public' OR c.owner_group_id = $2)` in place of
    /// `c.visibility::text = 'public'` (moving the splice's first group
    /// parameter to 3), then flip `wiki_generate_counts_only_public_members_until_ke1`
    /// in `episcience-api`'s `mcp_write_tools_test`.
    ///
    /// The kernel's `ClaimThemeRepository::get_summary` with `properties`
    /// added, on any executor (the request path passes its stamped
    /// transaction; that kernel method takes a pool). The theme row itself
    /// is not a tenancy row; the member count also carries the viewer's
    /// `/* {VISIBILITY:c} */` splice (a no-op on public rows), so no change to
    /// the visibility predicate can ever count a member the viewer cannot read.
    pub async fn theme_for_wiki_as<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        viewer: &Viewer,
        theme_id: Uuid,
    ) -> Result<Option<WikiThemeSource>, DbError> {
        let sql = viewer.splice(
            "SELECT t.label, t.description, t.properties, \
                    COALESCE(m.member_count, 0)::int8 AS public_members \
               FROM public.claim_themes t \
               LEFT JOIN LATERAL ( \
                   SELECT COUNT(*) AS member_count FROM public.claims c \
                    WHERE c.theme_id = t.id AND COALESCE(c.is_current, true) \
                      AND c.visibility::text = 'public' \
                      /* {VISIBILITY:c} */ \
               ) m ON TRUE \
              WHERE t.id = $1",
            2,
        );
        let mut q =
            sqlx::query_as::<_, (String, String, serde_json::Value, i64)>(&sql).bind(theme_id);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        Ok(q.fetch_optional(executor).await?.map(
            |(label, description, properties, public_members)| WikiThemeSource {
                label,
                description,
                properties,
                public_members,
            },
        ))
    }

    /// The registered Ed25519 SIGNING key of agent `id` (kernel
    /// `agents.public_key`), or `None` when there is no such agent or its key
    /// is not a signing key. The kernel's rule for every signature path
    /// (`agents.key_kind` comment; `AgentRepository::public_key_if_signer`):
    /// only `key_kind = 'ed25519'` is a verifier; a `derived` key is a
    /// placeholder for a keyless OAuth principal that no one holds. A
    /// countersignature's `signer_id` is proven by a signature that verifies
    /// against THIS key, never a key the request supplies.
    pub async fn agent_public_key<'e, E: sqlx::PgExecutor<'e>>(
        executor: E,
        id: Uuid,
    ) -> Result<Option<Vec<u8>>, DbError> {
        Ok(sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT public_key FROM agents WHERE id = $1 AND key_kind = 'ed25519'",
        )
        .bind(id)
        .fetch_optional(executor)
        .await?)
    }
}
