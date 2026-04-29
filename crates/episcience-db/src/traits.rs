use async_trait::async_trait;
use uuid::Uuid;

use crate::errors::DbError;

/// Generic repository trait for fetching entities by primary key.
///
/// # Note
/// The current repo structs are zero-sized unit structs with static methods
/// that accept `&PgPool` as a parameter. Implementing this trait on them would
/// require the structs to carry a pool reference. Implementations will be added
/// once the repo structs are refactored to hold a pool reference in a constructor.
///
/// TODO: Implement Repository<T> for each repo struct once repos hold a pool
/// reference rather than accepting pool as a parameter.
#[async_trait]
pub trait Repository<T> {
    async fn get_by_id(&self, id: Uuid) -> Result<T, DbError>;
}
