# Design: Replace EpiClaw's SQLite scheduler with EpiGraph workflow entities

**Status:** design-only. LARGE, cross-repo, very-high blast radius. Explicitly
flagged design-only by the handoff and the advisor. No code in this PR.

**Source:** "Out of scope" #3 of
`docs/superpowers/plans/2026-05-28-epiclaw-episcience-integration.md`.

---

## The load-bearing risk (read this first)

The entity mapping (scheduled-task-row → workflow-node) is the easy part. The
hard, load-bearing part is **crash-recovery and persistence semantics**. EpiClaw's
scheduler is the component that must *not* lose a job, must *not* double-fire a
job, and must recover deterministically after a crash mid-tick. Moving that
state out of a local, fsync-on-commit SQLite file and onto a networked graph
service changes the failure model from "local disk durability" to "distributed
durability + network partition tolerance". The design must answer the recovery
questions before it answers the modeling questions.

---

## Current state (all EpiClaw-resident, read-only here)

- `epiclaw-host/src/host/scheduler_db.rs` — `rusqlite` over `scheduler.db`,
  `scheduled_tasks` table: `cron` / `interval` / `once`, `next_run` / `last_run`,
  `workflow_id`, `synthesis_skill`, prompt-sections.
- `epiclaw-host/src/host/scheduler.rs` — ~1385 LOC: `schedules.toml` seeding, a
  60 s poll loop, container enqueue, silent-token mint, `workflow_run_hook`
  wiring.
- No episcience-side scheduler exists.

**Concrete gap:** scheduling state lives in operational SQLite, not as EpiGraph
workflow entities. Cron/interval/once semantics, due-time computation, and the
poll loop are all local.

---

## Crash-recovery / persistence semantics (the part that must be solved)

| Property | SQLite today | On EpiGraph workflows (must preserve) |
|---|---|---|
| **Durability of "task is due"** | fsync on commit to local disk | a graph write; must be acked before the tick is considered recorded |
| **At-most-once / at-least-once fire** | single poller reads+updates `next_run` in one tx | needs a claim/lease (compare-and-set on `next_run`) so two pollers — or a poller + a retry — don't double-fire |
| **Crash mid-tick** | restart re-reads `next_run`; idempotent by row state | restart must re-derive due-state from the graph; an in-flight enqueue that crashed before recording `last_run` must be safely re-tried, not duplicated |
| **Clock/`next_run` advance** | computed locally, written in same tx as fire | the advance must be atomic with the fire-record, or a window opens for double-fire / missed-fire |
| **Backfill after downtime** | `next_run < now()` rows fire on next poll | same, but graph read latency must not cause a thundering herd of overdue jobs on restart |

The single hardest invariant: **advancing `next_run` and recording the fire must
be one atomic step.** SQLite gives this for free (one transaction). EpiGraph
workflow entities give it only if there is a conditional update (CAS on
`next_run`) or an explicit lease. Without it, the scheduler either double-fires
(advance after fire, crash in between → re-fire) or drops jobs (advance before
fire, crash in between → never fire).

---

## Entity mapping (the easy part)

- `scheduled_task` row → an EpiGraph **workflow** node carrying schedule metadata
  (kind, cron/interval expression, `next_run`, `last_run`, target skill,
  prompt-sections) as workflow properties or a typed schedule sub-entity.
- Each fire → a **workflow execution / run** record (episcience already models
  `workflow_run`; reuse that vocabulary so the scheduler's history is queryable
  in the same graph as synthesis provenance).
- `schedules.toml` seeding → a reconcile-by-query bootstrap (read desired
  schedules, upsert workflow nodes), matching the reconciler pattern used
  elsewhere in the EpiClaw host.

---

## Migration path (incremental, reversible at each step)

1. **Dual-write, read-from-SQLite.** Mirror `scheduled_tasks` mutations to
   EpiGraph workflow nodes; the SQLite poller stays authoritative. Validates the
   entity mapping with zero behavior change and zero recovery risk.
2. **Shadow-read.** A read-only second poller derives due-state from the graph
   and logs disagreements with the SQLite poller. No firing from the graph.
   Surfaces clock/atomicity gaps before they can cause a misfire.
3. **Cut over the lease.** Introduce the CAS/lease on `next_run` in the graph and
   make the graph poller authoritative for *one* low-stakes schedule. Keep SQLite
   as a warm fallback.
4. **Full cutover + retire SQLite.** Only after the lease invariant is proven
   under induced crashes (kill the host mid-tick, assert no double-fire / no
   drop).

Each step is independently revertible; never delete `scheduler.db` until step 4
has soaked.

---

## Risks

- **Double-fire / dropped jobs** if the advance+fire atomicity is not preserved
  (see table). This is the keystone risk; everything else is secondary.
- **Network dependency in the run loop.** A graph-unreachable window now stalls
  *all* scheduling, where SQLite never had this dependency. Needs a local
  degraded-mode (e.g. last-known schedule cache) or an explicit "scheduler
  paused" semantic.
- **Latency / thundering herd** on restart backfill against a networked store.
- **Blast radius:** touches EpiClaw's core run loop, persistence, crash-recovery
  semantics, and *every* scheduled job. A regression here silently stops all
  automation.

---

## Recommendation

Do not undertake this as a single refactor. If pursued, gate it behind the
4-step migration above, and treat step 3 (the lease/CAS atomicity) as the real
deliverable — the entity modeling is trivial by comparison. Until there is a
concrete operational pain (e.g. wanting scheduler history queryable in the graph,
or multi-host scheduling), the SQLite scheduler is the correct, lower-risk
choice and this stays deferred.
