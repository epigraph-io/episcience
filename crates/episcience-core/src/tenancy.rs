//! Ownership of EpiScience rows: the kernel's tenancy pair.
//!
//! Every EpiScience tenancy row carries `(owner_group_id, visibility)`, with
//! the kernel's meaning: members of the owner group read (and, as admin or
//! writer, edit) the row; `public` rows are readable by everyone. A ROOT row
//! (a synthesis, sample, protocol, or a blob without a sample) declares its
//! pair on insert; a DERIVED row takes its parent's pair, always.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use crate::synthesis::Visibility;

/// The pair a root write declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ownership {
    pub owner_group_id: Uuid,
    pub visibility: Visibility,
}

impl Ownership {
    /// Owned by `owner_group_id`, readable by its members only.
    #[must_use]
    pub const fn group(owner_group_id: Uuid) -> Self {
        Self {
            owner_group_id,
            visibility: Visibility::Group,
        }
    }

    /// Owned by `owner_group_id`, readable by everyone.
    #[must_use]
    pub const fn public(owner_group_id: Uuid) -> Self {
        Self {
            owner_group_id,
            visibility: Visibility::Public,
        }
    }

    /// `owner_group_id` with the given visibility.
    #[must_use]
    pub const fn new(owner_group_id: Uuid, visibility: Visibility) -> Self {
        Self {
            owner_group_id,
            visibility,
        }
    }
}
