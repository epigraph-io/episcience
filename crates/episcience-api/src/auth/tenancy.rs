//! Ownership decisions for EpiScience writes: the application half of the
//! tenancy model (the database half is the row guards of migration 5035).
//!
//! A human's EpiScience data is owned like their kernel data: by a GROUP.
//! A root write (a synthesis, sample, protocol, or a blob without a sample)
//! is owned by the group the request names, if the caller may write it, and
//! otherwise by the caller's default group (the kernel's
//! `ClaimRepository::default_decl_for_author`: the operator's group for an
//! operated agent, else the caller's own personal group). A read shows a row
//! to members of its owner group, or to everyone when it is `public`; an edit
//! needs `admin` or `writer` in the owner group. Authorship (`agent_id`,
//! `prepared_by`, ...) is recorded, never consulted for access.

use epigraph_core::TenancyDecl;
use epigraph_db::{ClaimRepository, Viewer};
use episcience_core::synthesis::Synthesis;
use episcience_core::{Ownership, Sample, Visibility};
use episcience_db::ProtocolRepository;
use serde::Deserialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::errors::ApiError;

/// The message every retired-share surface returns (HTTP 410).
pub const SHARES_RETIRED: &str =
    "synthesis shares are retired: own the synthesis in a team group (owner_group_id) instead";

/// A visibility as a request may spell it. `private` is an alias of `group`;
/// `shared` is retired (410): the kernel's only sharing primitive is group
/// ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestedVisibility {
    Group,
    Private,
    Public,
    Shared,
}

impl RequestedVisibility {
    /// The stored visibility, or 410 for the retired `shared`.
    pub fn resolve(self) -> Result<Visibility, ApiError> {
        match self {
            Self::Group | Self::Private => Ok(Visibility::Group),
            Self::Public => Ok(Visibility::Public),
            Self::Shared => Err(ApiError::Gone(SHARES_RETIRED.into())),
        }
    }

    /// Parse the MCP spelling (a plain string).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "group" => Ok(Self::Group),
            "private" => Ok(Self::Private),
            "public" => Ok(Self::Public),
            "shared" => Ok(Self::Shared),
            other => Err(format!(
                "visibility must be one of: group | public (\"private\" is accepted as group); got \"{other}\""
            )),
        }
    }
}

/// A body identity field (`prepared_by`, `authored_by`, `uploader_id`,
/// `agent_id`): optional, and when present it must be the authenticated
/// caller (403 otherwise). The database binds the author to the principal
/// anyway (the author guard of 5035); this keeps the answer a clear 403
/// rather than a guard refusal, for one release.
pub fn bound_identity(
    field: &str,
    supplied: Option<Uuid>,
    principal: Uuid,
) -> Result<Uuid, ApiError> {
    match supplied {
        Some(id) if id != principal => Err(ApiError::Forbidden(format!(
            "{field} must be the authenticated caller (omit it to default to the caller)"
        ))),
        _ => Ok(principal),
    }
}

/// The caller's default owner group: the kernel's own answer for a claim the
/// caller would author (`default_decl_for_author`).
pub async fn default_group(conn: &mut PgConnection, principal: Uuid) -> Result<Uuid, ApiError> {
    let decl = ClaimRepository::default_decl_for_author(conn, principal)
        .await
        .map_err(|e| ApiError::Internal(format!("resolve the caller's default group: {e}")))?;
    decl.owner_group_bind()
        .ok_or_else(|| ApiError::Internal("the caller's default declaration names no group".into()))
}

/// The pair of a ROOT write. A named `owner_group_id` must be one of the
/// caller's writable groups (403 before anything is written); without one,
/// the caller's default group.
pub async fn root_ownership(
    conn: &mut PgConnection,
    viewer: &Viewer,
    requested_group: Option<Uuid>,
    visibility: Visibility,
) -> Result<Ownership, ApiError> {
    let principal = viewer
        .principal()
        .ok_or_else(|| ApiError::Forbidden("a write needs an authenticated principal".into()))?;
    let group = match requested_group {
        Some(g) if viewer.writable_groups().contains(&g) => g,
        Some(_) => {
            return Err(ApiError::Forbidden(
                "owner_group_id is not a group the caller may write (admin or writer)".into(),
            ))
        }
        None => default_group(conn, principal).await?,
    };
    Ok(Ownership::new(group, visibility))
}

/// The pair of a synthesis created under `parent` (a refinement). A child of
/// a NON-public parent stays in the parent's owner group: the caller must be
/// able to write that group and may not name another (403; the database
/// refuses the same with 42501), and it is `group` whatever the request asked
/// (the database stores the same from 5035 on; a public child would copy the
/// parent's query into a world-readable row). A public parent does not
/// constrain the child.
pub async fn child_ownership(
    conn: &mut PgConnection,
    viewer: &Viewer,
    parent: &Synthesis,
    requested_group: Option<Uuid>,
    visibility: Visibility,
) -> Result<Ownership, ApiError> {
    if parent.visibility == Visibility::Public {
        return root_ownership(conn, viewer, requested_group, visibility).await;
    }
    let parent_group = parent
        .owner_group_id
        .ok_or_else(|| ApiError::NotFound(format!("synthesis {} not found", parent.id)))?;
    if requested_group.is_some_and(|g| g != parent_group) {
        return Err(ApiError::Forbidden(
            "a refinement of a non-public synthesis is owned by the parent's group".into(),
        ));
    }
    if !viewer.writable_groups().contains(&parent_group) {
        return Err(ApiError::Forbidden(
            "refining a non-public synthesis needs write access to its owner group".into(),
        ));
    }
    Ok(Ownership::new(parent_group, Visibility::Group))
}

/// Fail closed at birth: a synthesis asked to be `public` whose prerequisites
/// are not all public is stored `group` (the database's insert guard does the
/// same from 5035 on; this covers the deploy window before it). The caller
/// has already established it can read every prerequisite.
pub async fn narrow_for_prerequisites(
    conn: &mut PgConnection,
    viewer: &Viewer,
    visibility: Visibility,
    prerequisites: &[Uuid],
) -> Result<Visibility, ApiError> {
    if visibility != Visibility::Public {
        return Ok(visibility);
    }
    for id in prerequisites {
        let p = episcience_db::SynthesisRepository::get_readable(&mut *conn, *id, viewer).await?;
        if p.visibility != Visibility::Public {
            return Ok(Visibility::Group);
        }
    }
    Ok(visibility)
}

/// The tenancy declaration of an observation claim: as private as its sample
/// (`('group', <the sample's group>)`) when the sample is `group`; otherwise
/// `public`, owned by the author's default group. Never the world or seed
/// group.
pub async fn observation_decl(
    conn: &mut PgConnection,
    sample: &Sample,
    author: Uuid,
) -> Result<TenancyDecl, ApiError> {
    match (sample.visibility, sample.owner_group_id) {
        (Some(Visibility::Group), Some(g)) => Ok(TenancyDecl::group(g)),
        _ => Ok(TenancyDecl::public(default_group(conn, author).await?)),
    }
}

/// The pair of a blob: a blob attached to a sample takes the sample's pair
/// (the database enforces the same); an unattached blob is a root, `public`
/// in the caller's default group.
pub async fn blob_ownership(
    conn: &mut PgConnection,
    viewer: &Viewer,
    sample: Option<&Sample>,
) -> Result<Ownership, ApiError> {
    match sample {
        Some(s) => match (s.owner_group_id, s.visibility) {
            (Some(g), Some(v)) => Ok(Ownership::new(g, v)),
            _ => Err(ApiError::NotFound(format!("sample {} not found", s.id))),
        },
        None => root_ownership(conn, viewer, None, Visibility::Public).await,
    }
}

/// The pair of a protocol (`public`, in the requested or default group). A
/// supersede needs WRITE access to the superseded protocol's group: a new
/// version of someone else's protocol is a fork (a new root), not a
/// supersede. 404 when the caller cannot read the superseded protocol, 403
/// when it can read but not write it.
pub async fn protocol_ownership(
    conn: &mut PgConnection,
    viewer: &Viewer,
    supersedes: Option<Uuid>,
    requested_group: Option<Uuid>,
) -> Result<Ownership, ApiError> {
    if let Some(prev) = supersedes {
        ProtocolRepository::get_readable(&mut *conn, prev, viewer).await?;
        if !ProtocolRepository::writable_by(&mut *conn, prev, viewer).await? {
            return Err(ApiError::Forbidden(
                "superseding a protocol needs write access to its owner group; publish a new protocol (a fork) instead"
                    .into(),
            ));
        }
    }
    root_ownership(conn, viewer, requested_group, Visibility::Public).await
}

/// The pair of a countersignature of a claim with the given pair (brief 7.1):
/// a PUBLIC claim's attestation is `public`, owned by the writer's default
/// group; a GROUP claim's attestation is `('group', <the claim's group>)`,
/// and the writer must be able to write that group (403 otherwise). The
/// caller has already established that it can read the claim.
pub async fn countersign_ownership(
    conn: &mut PgConnection,
    viewer: &Viewer,
    claim_visibility: &str,
    claim_owner: Uuid,
) -> Result<Ownership, ApiError> {
    if claim_visibility == "public" {
        return root_ownership(conn, viewer, None, Visibility::Public).await;
    }
    if !viewer.writable_groups().contains(&claim_owner) {
        return Err(ApiError::Forbidden(
            "countersigning a group claim needs write access to the claim's group".into(),
        ));
    }
    Ok(Ownership::group(claim_owner))
}

/// The STRICT Ed25519 check every countersignature path uses: the key must
/// decode to a curve point that is not of small order (a "weak" key: any
/// signature with a small-order `R` and `s = 0` verifies against it under the
/// cofactorless check), and the signature must pass
/// `VerifyingKey::verify_strict` (which also refuses a small-order `R` and a
/// non-canonical `s`). An honest signature passes both.
pub fn verify_ed25519_strict(key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(key) else {
        return false;
    };
    if vk.is_weak() {
        return false;
    }
    vk.verify_strict(message, &ed25519_dalek::Signature::from_bytes(signature))
        .is_ok()
}

/// Verify a version-2 countersignature: the Ed25519 signature over
/// `claim_id|signer_id|meaning|content` must STRICTLY verify
/// ([`verify_ed25519_strict`]) against `signer_id`'s REGISTERED signing key
/// (`key_kind = 'ed25519'`; a `derived` placeholder has no holder). A
/// request-supplied key, if any, must equal it. Returns the content hash to
/// store.
pub async fn verify_countersignature(
    conn: &mut PgConnection,
    claim_id: Uuid,
    signer_id: Uuid,
    meaning: &str,
    content: &str,
    signature: &[u8; 64],
    supplied_key: Option<&[u8; 32]>,
) -> Result<[u8; 32], ApiError> {
    let key = episcience_db::KernelClaimRepository::agent_public_key(conn, signer_id)
        .await?
        .ok_or_else(|| {
            ApiError::Validation(format!(
                "signer {signer_id} is not an agent with a registered signing key"
            ))
        })?;
    let key: [u8; 32] = key.as_slice().try_into().map_err(|_| {
        ApiError::Validation("the signer's registered key is not an Ed25519 key".into())
    })?;
    if supplied_key.is_some_and(|k| *k != key) {
        return Err(ApiError::Validation(
            "public_key_hex is not the signer's registered key".into(),
        ));
    }
    let canonical = format!("{claim_id}|{signer_id}|{meaning}|{content}");
    if !verify_ed25519_strict(&key, canonical.as_bytes(), signature) {
        return Err(ApiError::Validation(
            "Ed25519 signature verification failed".into(),
        ));
    }
    Ok(epigraph_crypto::ContentHasher::hash(canonical.as_bytes()))
}
