# Design: Phase 12 — episcience → EpiClaw "synthesis ready" push

**Status:** design-only (brainstorm before building). Cross-repo; the receiver
lives in EpiClaw, which is read-only in this environment and cannot be built
here.

**Source:** "Out of scope" #1 of
`docs/superpowers/plans/2026-05-28-epiclaw-episcience-integration.md`.

---

## The decision that is actually live

There are two ways to close the return path, and they are not equal. **Lead with
the recommendation: ride the existing EpiGraph event bus, do not build a bespoke
webhook.** The rest of this doc justifies that lean and records the webhook
alternative so the choice is legible.

### Current state (one-way integration)

- EpiClaw → episcience is wired. `WorkflowRunHook::on_task_completed`
  (`epiclaw-host/src/host/workflow_run_hook.rs`) records a `workflow_run` sample
  and enqueues a synthesis via `EpiscienceClient`
  (`epiclaw-host/.../episcience_client.rs`).
- episcience then runs the pipeline **async** and never notifies EpiClaw. The
  only completion-observation path is **poll-based**: the MCP `synthesize` tool
  (`crates/episcience-api/src/mcp/synthesize.rs`) has `wait_for_completion` + a
  2 s-cadence poll loop clamped at 600 s.
- episcience **already consumes** the EpiGraph event bus: a long-poll client
  (`crates/episcience-api/src/clients/epigraph_events.rs`) does
  `GET /api/v1/events` and reacts to upstream `belief.updated` etc.
- episcience has an in-memory `EventStore` and a `publish_event` surface but
  emits **no outbound notification** to EpiClaw on synthesis completion. There
  is no notify/callback/webhook in `synthesis_job.rs`.

**Concrete gap:** no push channel from episcience back to EpiClaw when a
synthesis reaches `status='complete'`. The EpiClaw fire-and-forget enqueue has no
return-path subscription.

---

## Option A (recommended) — Ride the EpiGraph event bus

episcience publishes a `synthesis.complete` (and `synthesis.failed`) event to
EpiGraph via its existing `publish_event` surface at the point where
`synthesis_job.rs` transitions a row to `complete`. EpiClaw subscribes by
long-polling `GET /api/v1/events` — **mirroring the exact consumer episcience
itself already runs** in `epigraph_events.rs`.

Why this is the better-grounded option:

1. **Both halves already exist as proven patterns.** The producer
   (`publish_event`) and the consumer (long-poll `GET /api/v1/events`) are both
   live in episcience today. We are not inventing a transport; we are adding one
   event type and one subscriber.
2. **No net-new auth surface.** A bespoke webhook needs a new episcience→EpiClaw
   egress path, a new EpiClaw ingress endpoint, and a shared secret / mTLS
   between two services that currently never call each other directly. The event
   bus already carries auth at the EpiGraph boundary both services trust.
3. **Idempotency + replay come for free.** The bus has ordered, cursor-based
   delivery; EpiClaw can checkpoint its cursor and survive restarts without
   missed or double-delivered notifications. A webhook would need
   at-least-once + dedup re-implemented on both ends.
4. **No new failure mode in the synthesis hot path.** Publishing to a bus
   episcience already publishes to is lower blast radius than a synchronous
   outbound HTTP call from `synthesis_job.rs` to EpiClaw (which would couple
   synthesis completion latency to EpiClaw availability unless made fully async
   with its own retry queue — i.e. re-deriving the bus).

### Interfaces (Option A)

- **episcience (buildable here, single-repo):**
  - Emit `publish_event(kind="synthesis.complete", subject=synthesis_id,
    payload={ status, agent_id, query, workflow_run_id?, parent_synthesis_id?,
    verification_outcome })` at the `complete` transition in
    `crates/episcience-api/src/jobs/synthesis_job.rs`. Emit
    `synthesis.failed` symmetrically at the `failed` transition with
    `failure_reason`.
  - The event payload must carry whatever correlation key EpiClaw needs to map
    the notification back to the originating `workflow_run` (the enqueue side
    already knows the synthesis id; thread a `workflow_run_id` through the
    enqueue → job → event so EpiClaw can reconcile without a reverse lookup).
- **EpiClaw (read-only here — design only):**
  - Add a long-poll subscriber for `synthesis.complete` / `synthesis.failed`,
    reusing the cursor-checkpoint pattern from episcience's own
    `epigraph_events.rs`.
  - On receipt, resolve the `workflow_run` and take the completion action
    (notify operator / unblock a gated step / update scheduler `last_run`).

### Open questions (Option A)

- Does EpiGraph's event schema allow a `synthesis.*` event kind, or is the kind
  enum closed? (episcience already publishes events, so likely open — verify
  against the EpiGraph `publish_event` contract before building the producer.)
- Event retention / cursor TTL: if EpiClaw is down longer than retention, it
  misses completions. Acceptable for a notification (re-poll syntheses on
  startup) but must be stated.

---

## Option B (rejected for now) — Bespoke webhook

episcience POSTs `synthesis.ready` directly to a new EpiClaw ingress endpoint.

- **New outbound egress** from episcience + **new ingress handler** in EpiClaw.
- **New auth** between the two services (shared bearer / mTLS).
- **Retry + idempotency** re-implemented on the push (at-least-once delivery,
  dedup key on the synthesis id).
- Couples synthesis completion to EpiClaw reachability unless fully queued —
  at which point it has re-derived a worse event bus.

Keep this option on file only if a future requirement needs a notification
EpiGraph's bus structurally cannot carry (e.g. a payload EpiClaw must receive
without the synthesis ever touching the graph). No such requirement exists today.

---

## Recommendation

Build Option A when Phase 12 is scheduled. The single-repo, buildable-here slice
is the **producer**: emit `synthesis.complete` / `synthesis.failed` via
`publish_event` from `synthesis_job.rs`, carrying the `workflow_run` correlation
key. The EpiClaw subscriber is a separate, EpiClaw-resident task that mirrors
`epigraph_events.rs`. Do not build the producer speculatively ahead of the
subscriber: an event with no consumer is dead infrastructure, and the
correlation-key shape should be fixed by the consumer's needs.
