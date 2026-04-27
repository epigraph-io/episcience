# Task 0.3 — Event Emission Status

| Event | Status | Notes |
|-------|--------|-------|
| `edge.added` | shipped | POST `/edges` handler, `crates/epigraph-api/src/routes/edges.rs` |
| `edge.deleted` | shipped | DELETE `/edges/:id` handler, same file |
| `claim.superseded` | shipped | Emitted inside POST `/edges` when `relationship.eq_ignore_ascii_case("supersedes")` |
| `frame.changed` | deferred | No hypothesis-set mutation endpoint exists; `refine_frame` creates a child frame (new row), it does not restructure an existing frame's hypotheses. Emit when a PATCH `/frames/:id/hypotheses` endpoint is added in a future task. |

## Test coverage limitation (known)

The `crates/epigraph-api/tests/events/staleness_events_test.rs` tests bypass the HTTP handler and exercise only the `EventStore` infrastructure plus payload schemas. A regression that removed an `event_store.push(…)` call from the POST `/edges` or DELETE `/edges/:id` handler would NOT be caught by these unit tests.

**Compensating coverage:** Phase 4's `StalenessWorker` integration suite (Task 4.2 onwards) exercises the full publish → consume → mark-stale path end-to-end against a live test server, which would catch a missing emission via downstream behavior.

**Why not closed here:** A handler-exercising test would need either (a) a live database for `edge_repository.create()` to succeed, or (b) a mock `AppState` with a swappable repository — neither pattern exists in the test infrastructure today, and adding one expands Task 0.3's scope materially. Phase 4's integration suite is the natural place to verify emissions fire correctly.

## Phase 4 must-include (carryover from Task 0.3 review)

**Required in Task 4.2's test plan:** at least one integration test that calls `POST /edges` through the HTTP stack and asserts an `edge.added` event appears in the EventStore. Same for `DELETE /edges/:id` → `edge.deleted` and a supersedes edge → `claim.superseded`. Without this, the Task 0.3 TDD gap becomes permanent.
