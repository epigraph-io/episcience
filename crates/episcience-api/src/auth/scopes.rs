//! The one scope table for EpiScience (REST and MCP).
//!
//! EpiScience accepts kernel-minted access tokens and enforces the kernel's
//! claim scopes on its own surface:
//!
//! - REST: a `GET`/`HEAD` needs [`CLAIMS_READ`]; every other method needs
//!   [`CLAIMS_WRITE`]. The fallback arm is the WRITE scope, so a method nobody
//!   thought about fails closed.
//! - MCP: every tool is listed in [`MCP_TOOL_SCOPES`]. A tool name absent from
//!   the table is refused (fail closed); a unit test pins the table to the
//!   tool router so adding a tool without a scope breaks the build's tests.
//!
//! Scopes are matched exactly, like the kernel's `AuthContext::has_scope`:
//! `claims:write` does not imply `claims:read`.

use axum::http::Method;

/// Read access to EpiScience rows and the kernel claims they surface.
pub const CLAIMS_READ: &str = "claims:read";

/// Any EpiScience write (rows here, and the claims an observation creates).
pub const CLAIMS_WRITE: &str = "claims:write";

/// The scope a REST request needs, by HTTP method.
#[must_use]
pub fn rest_required_scope(method: &Method) -> &'static str {
    if method == Method::GET || method == Method::HEAD {
        CLAIMS_READ
    } else {
        CLAIMS_WRITE
    }
}

/// `(tool name, required scope)` for every EpiScience MCP tool.
pub const MCP_TOOL_SCOPES: &[(&str, &str)] = &[
    ("recall_synthesis", CLAIMS_READ),
    ("get_synthesis", CLAIMS_READ),
    ("list_syntheses", CLAIMS_READ),
    ("list_countersignatures", CLAIMS_READ),
    ("synthesize", CLAIMS_WRITE),
    ("propose_protocol", CLAIMS_WRITE),
    ("add_observation", CLAIMS_WRITE),
    ("countersign", CLAIMS_WRITE),
    ("attach_blob", CLAIMS_WRITE),
];

/// The scope an MCP tool needs, or `None` for a name the table does not know
/// (callers refuse `None`).
#[must_use]
pub fn mcp_required_scope(tool: &str) -> Option<&'static str> {
    MCP_TOOL_SCOPES
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, scope)| *scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_reads_need_read_and_everything_else_needs_write() {
        assert_eq!(rest_required_scope(&Method::GET), CLAIMS_READ);
        assert_eq!(rest_required_scope(&Method::HEAD), CLAIMS_READ);
        for m in [
            Method::POST,
            Method::PATCH,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert_eq!(rest_required_scope(&m), CLAIMS_WRITE, "{m}");
        }
    }

    #[test]
    fn unknown_mcp_tool_has_no_scope() {
        assert_eq!(mcp_required_scope("drop_everything"), None);
        assert_eq!(mcp_required_scope("synthesize"), Some(CLAIMS_WRITE));
        assert_eq!(mcp_required_scope("get_synthesis"), Some(CLAIMS_READ));
    }
}
