//! Property-based tests for `synthesis` data types (Phase 5 Task 5.1).
//!
//! Two property suites:
//!
//! 1. **Round-trip:** [`SubgraphSnapshot`] (and its [`BeliefIntervalEntry`]
//!    children) round-trip through `serde_json::Value` bit-for-bit. This
//!    pins the wire-format contract that the `syntheses.subgraph_snapshot`
//!    JSONB column relies on — we save snapshots as `serde_json::Value`
//!    in `SynthesisRepository::save_snapshot` and reload them in
//!    `SynthesisRepository::get_by_id`, so any silent serde drift would
//!    corrupt persisted state at read time.
//!
//! 2. **Read-predicate monotonicity:** [`read_predicate`] (the pure-Rust
//!    mirror of the SQL `readable_by` predicate) satisfies three core
//!    safety properties:
//!      a) granting a share never *removes* read access (monotonicity in
//!         `has_share`),
//!      b) the owner can always read their own synthesis regardless of
//!         visibility,
//!      c) `public` visibility is universally readable.
//!
//! The predicate is intentionally extracted as a pure Rust function in
//! `crate::synthesis::read_predicate` so we can property-test it directly,
//! without spinning up Postgres. The corresponding SQL clause lives in
//! `episcience-db/src/repos/synthesis.rs` (`readable_by` /
//! `list_readable_by`); a separate DB-backed test
//! (`phase01_e2e_test::test_readable_by_visibility_matrix`) pins the SQL
//! and Rust implementations to the same truth table.

use super::{read_predicate, BeliefIntervalEntry, SubgraphSnapshot, Visibility};
use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;
use uuid::Uuid;

// ─── Generators ─────────────────────────────────────────────────────────────

/// Bounded f64 strategy that excludes NaN/Inf/subnormal so PartialEq round-trip
/// asserts work without epsilon comparisons.
fn finite_f64() -> impl Strategy<Value = f64> {
    // proptest's `NORMAL | POSITIVE | NEGATIVE` excludes NaN, ±Inf, and
    // subnormals. Combined with a `prop_map` clamp into a "reasonable"
    // probability-ish range to avoid serialization edge cases (e.g. f64 values
    // near max/min that lose precision through the JSON text representation).
    use proptest::num::f64::{NEGATIVE, NORMAL, POSITIVE};
    (NORMAL | POSITIVE | NEGATIVE).prop_map(|x| {
        // Clamp to [-1e6, 1e6] so the JSON-decimal round-trip preserves bits.
        // `belief` / `plausibility` / `pignistic_prob` are always in [0, 1] in
        // production, but the on-the-wire serde representation has no such
        // constraint; we only need *some* bounded range to dodge f64↔string↔f64
        // precision loss.
        x.clamp(-1e6, 1e6)
    })
}

prop_compose! {
    fn arb_uuid()(bytes: [u8; 16]) -> Uuid {
        Uuid::from_bytes(bytes)
    }
}

prop_compose! {
    fn arb_belief_interval_entry()(
        claim_id in arb_uuid(),
        frame_id in proptest::option::of(arb_uuid()),
        belief in finite_f64(),
        plausibility in finite_f64(),
        pignistic_prob in finite_f64(),
        framed: bool,
    ) -> BeliefIntervalEntry {
        BeliefIntervalEntry { claim_id, frame_id, belief, plausibility, pignistic_prob, framed }
    }
}

/// Generate a timestamp in the safe `[2000-01-01, 2100-01-01]` range so we
/// dodge chrono's overflow-on-extreme-values edges and keep RFC3339 round-trip
/// stable. Sub-microsecond resolution is preserved because chrono's serde
/// formatter writes nanoseconds.
fn arb_datetime() -> impl Strategy<Value = DateTime<Utc>> {
    // 2000-01-01T00:00:00Z .. 2100-01-01T00:00:00Z
    (946_684_800i64..4_102_444_800i64, 0u32..1_000_000_000u32).prop_map(|(secs, nanos)| {
        Utc.timestamp_opt(secs, nanos).single().unwrap_or_else(|| Utc.timestamp_opt(secs, 0).unwrap())
    })
}

/// Generate a small-ish `serde_json::Value` for `traversal_config` so the
/// snapshot's "configurable opaque blob" is exercised across object/array/string/number/bool/null shapes.
fn arb_traversal_config() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        // Use i64 ints (which JSON preserves exactly) — float bit-for-bit
        // round-trip through JSON is a separate property and not what
        // `traversal_config` carries in production.
        any::<i32>().prop_map(|n| serde_json::Value::Number(serde_json::Number::from(n))),
        ".*".prop_map(serde_json::Value::String),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::Array),
            proptest::collection::hash_map("[a-z]{1,4}".prop_map(String::from), inner, 0..4)
                .prop_map(|m| {
                    let mut map = serde_json::Map::new();
                    for (k, v) in m {
                        map.insert(k, v);
                    }
                    serde_json::Value::Object(map)
                }),
        ]
    })
}

prop_compose! {
    fn arb_snapshot()(
        claim_ids in proptest::collection::vec(arb_uuid(), 0..6),
        edge_ids in proptest::collection::vec(arb_uuid(), 0..6),
        belief_intervals in proptest::collection::vec(arb_belief_interval_entry(), 0..6),
        traversal_config in arb_traversal_config(),
        captured_at in arb_datetime(),
    ) -> SubgraphSnapshot {
        SubgraphSnapshot { claim_ids, edge_ids, belief_intervals, traversal_config, captured_at }
    }
}

fn arb_visibility() -> impl Strategy<Value = Visibility> {
    prop_oneof![
        Just(Visibility::Private),
        Just(Visibility::Shared),
        Just(Visibility::Public),
    ]
}

// ─── Properties ─────────────────────────────────────────────────────────────

proptest! {
    /// `serde_json::to_value(&snap)` followed by `from_value` must yield an
    /// equal `SubgraphSnapshot`. This pins the JSONB wire format used by
    /// `syntheses.subgraph_snapshot`.
    #[test]
    fn snapshot_round_trips_through_json_value(snap in arb_snapshot()) {
        let v = serde_json::to_value(&snap).expect("serialize");
        let r: SubgraphSnapshot = serde_json::from_value(v).expect("deserialize");
        prop_assert_eq!(snap, r);
    }

    /// **Monotonicity in `has_share`:** granting a share row never *removes*
    /// read access. This guards the SQL predicate against future refactors
    /// that might (e.g.) put a share row in front of an `AND NOT visibility =
    /// 'private'` clause and accidentally make sharing *block* a read it
    /// would otherwise allow.
    #[test]
    fn share_never_revokes_read(
        visibility in arb_visibility(),
        owner_bytes: [u8; 16],
        agent_bytes: [u8; 16],
    ) {
        let owner = Uuid::from_bytes(owner_bytes);
        let agent = Uuid::from_bytes(agent_bytes);
        let r_no_share = read_predicate(visibility, owner, agent, false);
        let r_share = read_predicate(visibility, owner, agent, true);
        if r_no_share {
            prop_assert!(r_share, "granting a share must not revoke read access");
        }
    }

    /// **Owner always reads.** Independent of visibility or share state,
    /// the agent identified by `owner_id == agent_id` can always read.
    /// This guards against accidentally introducing an `AND visibility !=
    /// 'private'`-style mistake in the predicate.
    #[test]
    fn owner_always_reads(visibility in arb_visibility(), id_bytes: [u8; 16], has_share: bool) {
        let id = Uuid::from_bytes(id_bytes);
        prop_assert!(read_predicate(visibility, id, id, has_share));
    }

    /// **Public is universally readable.** Any agent reads a public
    /// synthesis, regardless of share state. Pins the existence-leak
    /// semantic: public means truly public, not "public-but-only-with-a-share-row".
    #[test]
    fn public_always_readable(
        owner_bytes: [u8; 16],
        agent_bytes: [u8; 16],
        has_share: bool,
    ) {
        let owner = Uuid::from_bytes(owner_bytes);
        let agent = Uuid::from_bytes(agent_bytes);
        prop_assert!(read_predicate(Visibility::Public, owner, agent, has_share));
    }

    /// **Private/Shared without a share row → only the owner reads.**
    /// This is the contrapositive of the above: anyone who is *not* the
    /// owner and *does not* have a share row must be denied for
    /// non-public visibilities.
    #[test]
    fn non_owner_without_share_blocked_unless_public(
        visibility in prop_oneof![Just(Visibility::Private), Just(Visibility::Shared)],
        owner_bytes: [u8; 16],
        agent_bytes: [u8; 16],
    ) {
        let owner = Uuid::from_bytes(owner_bytes);
        let agent = Uuid::from_bytes(agent_bytes);
        prop_assume!(owner != agent);
        let allowed = read_predicate(visibility, owner, agent, false);
        prop_assert!(!allowed, "stranger without share must be denied for non-public visibility");
    }
}
