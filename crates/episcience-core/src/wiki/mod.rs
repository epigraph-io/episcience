//! Wiki articles (plan: docs/superpowers/plans/2026-10-08-wiki-phase-b-articles.md).
//! An article is a `wiki_article` synthesis keyed by [`WikiKey`] (theme
//! `properties`, never the theme UUID — re-projection mints new ids).
pub mod key;
pub mod seed;
pub use key::{article_query, title_from_query, WikiKey};
pub use seed::{select_article_seeds, SeedCandidate};

pub const WIKI_SKILL_NAME: &str = "wiki_article";
/// Theme members pulled from the kernel before diversity selection.
pub const CANDIDATE_POOL: i32 = 400;
/// Seeds per article (the spike's recall limit; the cost budget of 20 LLM calls holds at ~13).
pub const SEED_BUDGET: usize = 50;
/// Cosine at or above which two candidates are the same fact restated.
pub const DUP_COSINE: f32 = 0.95;
/// MMR weight on relevance (1 - weight on novelty).
pub const MMR_LAMBDA: f32 = 0.7;
/// An article exists only when the generating group can read at least this many members.
pub const MIN_READABLE_MEMBERS: i64 = 20;
