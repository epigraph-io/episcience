//! The wiki page registry (plan 2026-10-08-wiki-phase-b-articles.md, Task 5):
//! a READ over `public.syntheses`, no table of its own. A page is
//! `(owner_group_id, wiki_key)` (migration 5042); its current article is the
//! latest COMPLETE `wiki_article` synthesis for that pair, and every row for
//! the pair (any status) is its history. A newer generation that failed, or is
//! still running, never displaces the article the page serves.
//!
//! Visibility: the caller passes its viewer-stamped connection (row security
//! applies) AND its [`Viewer`], whose predicate is spliced into every read,
//! like the other synthesis reads in this crate. A page of a group the viewer
//! cannot read is absent, exactly like a missing one.

use chrono::{DateTime, Utc};
use epigraph_db::Viewer;
use episcience_core::wiki::{title_from_query, WIKI_SKILL_NAME};
use serde::Serialize;
use uuid::Uuid;

use crate::errors::DbError;

pub struct WikiRepository;

/// One wiki page as listed: its current (latest complete) article.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WikiPageRow {
    pub owner_group_id: Uuid,
    pub wiki_key: String,
    /// The label line of the article's query (`title_from_query`).
    pub title: String,
    /// The synthesis the page serves.
    pub synthesis_id: Uuid,
    /// When that synthesis completed.
    pub generated_at: DateTime<Utc>,
    /// Set when the served article has drifted (stale pages stay listed).
    pub stale_since: Option<DateTime<Utc>>,
}

/// One generation of a page (any status).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct WikiVersion {
    pub synthesis_id: Uuid,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// A page with its served article's text and its full history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WikiPageDetail {
    pub page: WikiPageRow,
    pub narrative: String,
    /// Every generation for the page's key, newest first, any status.
    pub history: Vec<WikiVersion>,
}

#[derive(sqlx::FromRow)]
struct CurrentRow {
    owner_group_id: Uuid,
    wiki_key: String,
    query: String,
    id: Uuid,
    completed_at: DateTime<Utc>,
    stale_since: Option<DateTime<Utc>>,
    /// Read only by [`WikiRepository::get_page`]; the list leaves it NULL.
    narrative: Option<String>,
}

impl CurrentRow {
    fn into_page(self) -> (WikiPageRow, Option<String>) {
        let page = WikiPageRow {
            owner_group_id: self.owner_group_id,
            wiki_key: self.wiki_key,
            title: title_from_query(&self.query).to_string(),
            synthesis_id: self.id,
            generated_at: self.completed_at,
            stale_since: self.stale_since,
        };
        (page, self.narrative)
    }
}

impl WikiRepository {
    /// Every page `viewer` can read, each with its latest COMPLETE article
    /// (ties on `completed_at` broken by the newer id). Stale articles are
    /// listed, with `stale_since` set. Ordered by group, then key.
    pub async fn list_pages(
        conn: &mut sqlx::PgConnection,
        viewer: &Viewer,
    ) -> Result<Vec<WikiPageRow>, DbError> {
        let sql = viewer.splice(
            "SELECT DISTINCT ON (s.owner_group_id, s.wiki_key)
                    s.owner_group_id, s.wiki_key, s.query, s.id, s.completed_at,
                    s.stale_since, NULL::text AS narrative
               FROM public.syntheses s
              WHERE s.wiki_key IS NOT NULL
                AND s.status = 'complete'
                AND s.skill_name = $1
                /* {VISIBILITY:s} */
              ORDER BY s.owner_group_id, s.wiki_key, s.completed_at DESC, s.id DESC",
            2,
        );
        let mut q = sqlx::query_as::<_, CurrentRow>(&sql).bind(WIKI_SKILL_NAME);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let rows = q.fetch_all(&mut *conn).await?;
        Ok(rows.into_iter().map(|r| r.into_page().0).collect())
    }

    /// Page `(group_id, wiki_key)` as `viewer` sees it: `None` when the
    /// viewer cannot read the group's articles or no COMPLETE generation
    /// exists (a page with only failed or running generations is not a page).
    pub async fn get_page(
        conn: &mut sqlx::PgConnection,
        viewer: &Viewer,
        group_id: Uuid,
        wiki_key: &str,
    ) -> Result<Option<WikiPageDetail>, DbError> {
        let sql = viewer.splice(
            "SELECT s.owner_group_id, s.wiki_key, s.query, s.id, s.completed_at,
                    s.stale_since, s.narrative
               FROM public.syntheses s
              WHERE s.owner_group_id = $1
                AND s.wiki_key = $2
                AND s.status = 'complete'
                AND s.skill_name = $3
                /* {VISIBILITY:s} */
              ORDER BY s.completed_at DESC, s.id DESC
              LIMIT 1",
            4,
        );
        let mut q = sqlx::query_as::<_, CurrentRow>(&sql)
            .bind(group_id)
            .bind(wiki_key)
            .bind(WIKI_SKILL_NAME);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let Some(current) = q.fetch_optional(&mut *conn).await? else {
            return Ok(None);
        };
        let (page, narrative) = current.into_page();
        // `syntheses_check`: a complete row always has its narrative.
        let narrative = narrative.ok_or_else(|| {
            DbError::Constraint(format!(
                "synthesis {}: complete without a narrative",
                page.synthesis_id
            ))
        })?;

        let sql = viewer.splice(
            "SELECT s.id AS synthesis_id, s.status, s.created_at, s.completed_at
               FROM public.syntheses s
              WHERE s.owner_group_id = $1
                AND s.wiki_key = $2
                AND s.skill_name = $3
                /* {VISIBILITY:s} */
              ORDER BY s.created_at DESC, s.id DESC",
            4,
        );
        let mut q = sqlx::query_as::<_, WikiVersion>(&sql)
            .bind(group_id)
            .bind(wiki_key)
            .bind(WIKI_SKILL_NAME);
        if let Some(groups) = viewer.group_bind() {
            q = q.bind(groups);
        }
        let history = q.fetch_all(&mut *conn).await?;
        Ok(Some(WikiPageDetail {
            page,
            narrative,
            history,
        }))
    }
}
