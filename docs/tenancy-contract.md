# Tenancy contract

EpiScience's tables live in the EpiGraph kernel's database and adopt the
kernel's tenancy system (owner groups, visibility, row-level security, the
application and maintenance roles). The **tenancy contract** is the fixed,
versioned list of kernel objects EpiScience's SQL and runtime may rely on.
Nothing outside the list may be referenced; everything on it is asserted.

## Where it is asserted

| Place | What | Fails how |
|---|---|---|
| Migration 5033 | an inline check (the file's first statement) | the migration refuses, naming the first missing item; nothing of 5033 is created |
| `public.episcience_assert_kernel_contract(1)` | the same checks, created by 5033; **every later EpiScience migration calls it first** | that migration refuses, naming the item |
| Boot probe (`episcience_db::tenancy_contract::probe`) | the server and the MCP binary, right after connecting and before serving | the process exits non-zero, listing every failed item |
| `episcience-migrate verify` | the function above plus ledger consistency | non-zero exit (the deploy guard) |
| CI (`kernel_contract` tests) | all of the above against the schema the **pinned** kernel rev builds, plus a negative case per item except C1 (see the residuals register) | red build |
| Nightly canary (`.github/workflows/kernel-head-canary.yml`) | the same suite against the schema kernel **`main` HEAD** builds | red canary: the next pin bump would break (not a required check; it never runs on a pull request) |

The preamble and `verify` fail with `kernel contract v1: <item> failed: …`; the
boot probe fails with `tenancy contract v1 probe failed: <item>: expected …`
(every failed item, separated by `;`).

## Version 1 (migration 5033, `CONTRACT_VERSION = 1`)

All items exist at kernel migration head 110. "Preamble" = the 5033 check and
the assertion function; "probe" = the boot probe.

| Item | Kernel object | Why EpiScience needs it | Preamble | Probe |
|---|---|---|---|---|
| C1 | roles `epigraph_app`, `epigraph_maintenance` | the application role EpiScience runs on; the owner of its maintenance definers | yes | yes |
| C2 | `public.epigraph_bypass()`, `epigraph_definer_bypass()`, `epigraph_session_groups()`, `epigraph_writable_groups()`, `epigraph_principal_id()`, with their result types, each EXECUTE-able by `epigraph_app` | policies, row guards and the privileged-session test | yes | yes |
| C3 | `public.groups(id, kind, did_key, created_by_agent_id)`, `public.group_memberships(group_id, agent_id, role, revoked_at)` | owner groups and memberships | yes | yes |
| C4 | `public.claims(id, visibility, owner_group_id)` | claim-attach guards and visibility reads | yes | yes |
| C5 | `public.security_events(event_type, agent_id, success, details)`, INSERT by `epigraph_maintenance` | the audit rows maintenance definers append | yes | yes |
| C6 | the world and seed sentinel groups | refused as owners of EpiScience rows | yes | no (row content) |
| C7 | `public._sqlx_migrations(version, success)`, read-only: the successful head over kernel-range versions (below 5000) is >= 110, and version 110 itself is recorded as applied, so no foreign or out-of-range row can satisfy it | the kernel schema generation the contract was written against | yes | no (row content) |
| C8 | the `entity_types` registration of `synthesis` -> `syntheses`, read-only | kernel edge validation of synthesis endpoints | yes | no (row content) |
| C9 | extension `vector` in schema `public` | EpiScience's embedding columns are typed `public.vector` | yes | yes |
| C10 | `epigraph_maintenance` SELECT on `claims`, `groups`, `group_memberships` | maintenance definers read them | yes | yes |
| C11 | `epigraph_app` INSERT on `claims`, `edges`, `events` | in-process claims, PROV edges and events | yes | yes |
| C12 | `epigraph_app` EXECUTE on `epigraph_live_memberships(uuid)` and `epigraph_operator_of_author(uuid)` | viewer resolution; the operator-parity refusal | yes | yes |
| C13 | `epigraph_app` SELECT on `agents(id, public_key, display_name)` | countersignature verification, export | yes | yes |
| C14 | `epigraph_app` USAGE on `public.events_graph_version_seq` | event publishing (a missing grant would make events vanish silently) | yes | yes |
| L1 | `public.syntheses.autonomy_level` (EpiScience's own legacy head) | 5033 applies on top of the 5032 baseline only | yes | yes |
| S1 | `public.episcience_assert_kernel_contract(integer)` (EpiScience's own 5033) | a binary built for contract v1 refuses a database not migrated to it | no | yes |
| S2 | the connecting login itself: `pg_has_role(session_user, 'epigraph_app', 'USAGE')`, and its own INSERT on `claims`/`edges`/`events`, USAGE on the events sequence, EXECUTE on the C12 functions | C1-C14 check what the kernel grants `epigraph_app`; a login that is not an inheriting member of it (a missing grant, or NOINHERIT) would pass them all and lose writes silently | no | yes |

C11-C14 are probed at boot as well as asserted by the preamble because each
fails silently at run time; S2 then checks that the login the process
connected as actually holds those privileges. The probe uses only catalog reads and the
`has_*_privilege` inquiry functions, so it works on the non-superuser
application login; the three row-content items (C6-C8) are left to the
preamble, which runs as the migration owner.

**Rust contract** (compile-time, versioned by the kernel pin, see
`docs/kernel-pin.md`): `ScopedPool::{connect_with_options, begin_as, read_as,
probe_session_gucs}`, `SessionGucMode`, `Viewer::{resolve, splice,
splice_write, detach_scoped, writable_groups}`, `TenancyDecl`,
`ClaimRepository::{default_decl_for_author, create_conn}`,
`AgentRepository::operator_of_author`, `EdgeRepository::create`,
`EventRepository::publish_or_log_conn`, `epigraph_engine::{recall::recall,
belief_query::get_belief}`, `epigraph_auth::{JwtConfig, EpiGraphClaims,
assert_production_secret}`.

**Never referenced** (outside the contract): kernel functions newer than head
110, any kernel trigger function, the kernel's literal table arrays,
`epigraph_node_tenancy`, `epigraph_link_operator` and its siblings,
`epigraph_seed`, `tenancy_exempt`, `epigraph.allow_declassify`.

What the migration lint enforces of this, in code (comments excluded; bodies
and string literals included): every kernel `epigraph_*` name must be one of
the contract-v1 names (`epigraph_app`, `epigraph_maintenance`, the five C2
functions, the two C12 functions), which refuses the 114+ functions, the
kernel's prefixed tenancy trigger functions and the operator-link siblings;
no kernel `epigraph.*` setting may be named; and an explicit list refuses
`tenancy_exempt` and the kernel's unprefixed trigger functions at the pinned
head. The literal table arrays are not machine-checked (they are values, not
names); review covers them. The 5032 baseline predates the contract and keeps
the legacy `updated_at` triggers of `samples` and `protocols` on the kernel's
`update_updated_at_column()`; no later migration may reference it.

### Also created by 5033

- `public.episcience_session_is_privileged()` (SECURITY INVOKER, default
  EXECUTE): true when the current role is a superuser or has BYPASSRLS, when
  the session holds the kernel maintenance bypass (`epigraph_bypass()`,
  session_user), or when it runs inside a maintenance-owned definer
  (`epigraph_definer_bypass()`, current_user). EpiScience's row guards exempt
  only such sessions. It is plpgsql with early returns because Postgres checks
  EXECUTE on every function of an expression before evaluating it; a caller
  that reaches the last arm without EXECUTE on it gets an error, never true.
- NOLOGIN roles `episcience_rw`, `episcience_queue`, `episcience_maint_ops`
  (the grantees of EpiScience's table privileges and definers, which later
  migrations issue). Roles are cluster-scoped: each is created only when
  absent, and a pre-existing role of the same name is refused if any of the
  following holds (otherwise it is adopted; the list is what is checked, not
  a proof that the role equals a fresh one). It can log in or carries an
  elevated attribute; it is a member of any role (a fresh grantee is a member
  of nothing, which covers superuser roles, the predefined
  `pg_write_all_data`-class roles and every kernel role); it has a member
  other than the three EpiScience logins (apart from the admin-only grant
  PostgreSQL 16 gives a non-superuser creator); one of those logins is a
  member while being a superuser, BYPASSRLS or a kernel maintenance member;
  it already holds a privilege, an owned object, a policy or a per-role
  setting in this database or in the cluster's shared catalogs; or one of
  those logins has a member of its own. Each refusal is tested by running the
  block with throwaway role names (the kernel maintenance role included).
  Login roles are never created by a migration.

## Changing the contract

Adding, removing or changing an item is a new contract version:

1. a new migration that creates the check for `vN` (the assertion function
   gains the version) and asserts it first;
2. `CONTRACT_VERSION` bumped in `episcience_db::tenancy_contract`, with the
   probe's checks updated;
3. a new table in this file, and a negative test per new item;
4. a pin whose kernel provides every item (the canary shows this in advance).

Every EpiScience migration file from 5033 on is linted (`migration_lint`): it
opens with the contract assertion; never changes the session `search_path` and
pins exactly `search_path = public, pg_temp` on every function; qualifies every
object with `public.`; touches no kernel table, in top-level statements, DO
blocks, function bodies (where the maintenance definers' audit INSERT into
`security_events` is the one admitted kernel write) and dynamic SQL, with
comma-separated object lists read in full (one allowlisted detach excepted);
creates, alters or grants no role and switches no role (5033's own NOLOGIN
roles excepted), and issues no schema-, database- or cluster-level statement;
runs dynamic SQL only from a literal or a `%I`/`%L`-only `format()` literal;
never writes the kernel ledger; names only contract-v1 kernel objects; and
carries no uuid literal other than the two sentinels.

## Ownership model (migrations 5034, 5035)

Every EpiScience tenancy row carries the kernel's pair `(owner_group_id,
visibility)`: members of the owner group read it (`admin` / `writer` edit
it); a `public` row is readable by everyone. Authorship columns are recorded
and bound to the calling principal, never consulted for access.

| Class | Tables | Pair |
|---|---|---|
| ROOT | `syntheses`, `samples`, `protocols`, `blobs` without a sample | declared by the write (requested group if writable, else the caller's default group); a child of a non-public synthesis / group sample stays in the parent's group |
| DERIVED | the six synthesis children, `sample_claims`, `blobs` on a sample | always the parent's; follows the parent (propagation) |
| CLAIM-ATTACH | membership, `sample_claims`, `countersignatures` | the claim must be visible and public or owned by the row's group; a public sample takes public claims only |
| FROZEN | `synthesis_shares`, `episcience_worker_state` | no pair; the share routes answer 410 |

Row guards (5035) are SECURITY INVOKER and fire in name order: require /
inherit, parent-pinned (a row's parent, prerequisites, superseded protocol
and attached claim are fixed at insert), author, claim guard / job
principal, owner-immutable / derived-pin, widening guard (interlock
`episcience.allow_widen` plus every input public), publish rule, and one
maintenance-owned DEFINER: the statement-level propagation of a parent's pair
to its children (a child sample follows only while it carries the parent's
old pair; another group's child is never re-owned, and a change that would
leave it under a group parent is refused).

A synthesis is public only while every input is: one asked public whose
parent or a prerequisite is not public is stored `group` at birth; a
non-public member claim narrows it as it attaches (marked `input_narrowed`);
any status change re-checks it (an input narrowed since). The application
applies the same REFUSALS with the same words (a widening re-checks
publishability; an observation links only a claim the attach rule admits; a
synthesis' membership set citing a non-public claim of a group other than
the synthesis' is refused as a whole), and the same narrowing AT BIRTH (a
refinement of a non-public parent, or a synthesis with a non-public
prerequisite, is `group`). So every refusal, and every birth visibility, is
identical before the guards exist (the deploy window between the expand and
contract steps) and after. Narrowing LATER, as a non-public member of the
synthesis' own group attaches or at a status change, is the guards' alone:
in the window it waits for 5035's data step (residual "Deploy-window
completions"). 5035's own data step narrows, and derives, what was written
without the guards, and refuses (with a HINT) rows only the operator can
decide.

Legacy rows are re-owned by an audited one-shot (`episcience-maint
backfill-owners`, maintenance-owned definers of 5034) between the expand and
contract migrations. Its reverse accepts only a manifest whose hash an APPLIED
run recorded in its own audit rows, mapping visibility the way the backfill
does. `docs/runbooks/5035-undo.sql` is the compensating script for the
contract step; `docs/runbooks/e1c-rollback-vocabulary.sql` then converts the
vocabulary back for the previous binary.

Countersignatures: `countersigned_by` is the recording principal; `signer_id`
is proven by an Ed25519 signature that STRICTLY verifies (no small-order key
or `R`) against the signer's registered signing key (`agents.key_kind =
'ed25519'`; a `derived` placeholder key has no holder).

## Row security and the definer set (migrations 5036, 5037)

5036 enables AND forces row level security on all 14 tables. Every policy
opens with the kernel's two bypass arms (`epigraph_bypass()`,
`epigraph_definer_bypass()`), so the migration owner and the
maintenance-owned definers pass, and calls no function outside the
contract-v1 helpers.

| Kind | Tables | Policies |
|---|---|---|
| T-PUB | the ten ownership tables (`syntheses`, `samples`, `protocols`, `blobs`, the synthesis children except the job, `sample_claims`) | `<t>_tenancy` FOR ALL: read = public OR owned by a session group (the kernel viewer's order, so `/* {VISIBILITY:x} */` splices read the same rows); write = owned by a WRITABLE group. RESTRICTIVE `<t>_update_owner` and `<t>_delete_owner`: changing or removing a row needs write access to its current owner (a public row is readable by all, editable by its owners only) |
| T-PRIV | `synthesis_jobs` | read = owned by a session group (no public arm); insert = writable group AND the job acts as the session principal; UPDATE / DELETE: bypass arms only (the queue definers) |
| T-APPEND | `countersignatures` | read and insert as T-PUB; UPDATE / DELETE: bypass arms only |
| T-CTRL | `synthesis_shares`, `episcience_worker_state` | one FOR ALL policy, bypass arms only |
| R | `synthesis_claim_membership`, `sample_claims`, `countersignatures`, `synthesis_provo_edges` (claim targets) | RESTRICTIVE `<t>_claim_visible`: the cited claim must be readable under the SESSION's own row security on `claims`, so a claim narrowed out of a reader's reach hides every row citing it at once |

Grant matrix (every other grantee holds nothing on the 14; the kernel's
default privileges are revoked first):

| Tables | kernel app role | `episcience_rw` | kernel maintenance |
|---|---|---|---|
| the kept-SELECT set (the EpiScience tables the kernel's entity registry names; today `syntheses`) | SELECT | as below | as below |
| the ten ownership tables | none (except kept-SELECT) | S, I, U, D | S, I, U, D |
| `synthesis_jobs`, `countersignatures` | none | S, I | S, I, U, D |
| the two frozen tables | none | none | S, I, U, D |

`episcience_queue` and `episcience_maint_ops` hold no table privilege: their
whole authority is EXECUTE on the maintenance-owned definers. The closed
definer set (owner `epigraph_maintenance`, `search_path` pinned, EXECUTE
revoked from PUBLIC and granted to exactly one role):

| Definer | Migration | EXECUTE | Does |
|---|---|---|---|
| `episcience_maint_backfill_owners`, `episcience_maint_backfill_reverse` | 5034 | `episcience_maint_ops` | the one-shot legacy re-own and its manifest-bound reverse |
| `episcience_propagate_parent_tenancy` | 5035 | (trigger) | children follow their parent's pair |
| `episcience_queue_claim`, `_finish`, `_retry` | 5037 | `episcience_queue` | take the next due job (`SKIP LOCKED`); `running -> complete / failed`; `running -> queued` later, within the attempt limit; never a second job row |
| `episcience_owner_worklist` | 5037 | `episcience_queue` | (synthesis, job principal) pairs needing stage-6 edges or a staleness recheck; ids only |
| `episcience_countersign_chain_head` | 5037 | `episcience_rw` | the claim's latest signature whoever wrote it, under the per-claim transaction lock; refuses a claim the caller cannot read |
| `episcience_maint_sweep_narrowed` | 5037 | `episcience_maint_ops` | narrow-only: a public synthesis that stopped being publishable becomes `group` (to a fixpoint), marked `input_narrowed`, one staleness event and one audit row each |

Every row guard stays SECURITY INVOKER. `episcience-migrate verify` (the
deploy guard) refuses a database whose definer set, row-security flags, table
ACLs or ledger-schema ACL differ from the above, or that holds a row owned by
the world or seed sentinel, listing every finding. Ratchets R1-R5
(`crates/episcience-db/tests/{tenancy_coverage,owner_scoped_writes,policy_arms,privilege_matrix,definers}.rs`)
pin the same model from the tests' side; a future EpiScience table must be
added to the model (and its migration must repeat the REVOKE) or R1 and R4
fail.

The running processes are still privileged until they move to their own
logins, so row security changes nothing for them yet; the kernel application
role loses write access to the 14 tables. `docs/runbooks/episcience-rls-undo.sql`
(row security off, pre-5036 grants back; refuses while an EpiScience login is
connected) and `episcience-rls-redo.sql` are the tested compensating pair.

## Residuals register

Accepted residuals of the tenancy series, class-level. Each names what closes
it.

| Residual | Effect | Closed by |
|---|---|---|
| Revocation lag (B-S1) | a revoked human token keeps working at EpiScience until its expiry (at most one hour) | an audience-scoped EpiScience token issued by the kernel |
| Application-asserted session settings (B-S3) | the database-side principal checks catch EpiScience bugs; a compromised application or worker login could stamp any group on kernel tables. Only the maintenance login is narrow | not closable by EpiScience alone (kernel design) |
| Shared token secret | EpiScience verifies kernel tokens with the shared HMAC secret; the tenancy series confines it to the server and MCP units' environment | the audience-scoped key above |
| Narrowing lag (RS4 class) | a public synthesis whose input is narrowed out of band stays public until the narrowing sweep runs (its definer exists from 5037; its timer arrives with the worker split; minutes once it runs); text already copied into a narrative is not retracted | by design (privatization is not retroactive) |
| Chain head across writers | `episcience_countersign_chain_head` returns a claim's latest signature bytes to any caller who may read the claim, including the signature of an attestation the caller cannot read itself (the chain must span writers) | by design (a signature reveals that an attestation exists, not its content) |
| Guards behind a missing privilege | on the append-only tables (`synthesis_jobs`, `countersignatures`) the owner-immutable and derived-pin triggers are unreachable by any non-privileged session (no UPDATE privilege, bypass-only UPDATE policies); they stay as defence in depth | none needed |
| Published PROV edges after narrowing | a synthesis narrowed after publication keeps the kernel PROV edges already written (they name only its id and public endpoints) | by design |
| Legacy PROV edges | kernel PROV edges written before the tenancy series are world-owned and unsigned | not re-owned (kernel rows) |
| Blob hash oracle | the content-addressed blob store reveals whether content with a given hash exists | open |
| Kernel foreign keys (RS6) | `countersignatures.claim_id` (RESTRICT) and `sample_claims` (CASCADE) reference kernel claims | open |
| Public-only seeding | until the engine offers connection-scoped reads, the worker seeds and scores public claims only, so a principal's private claims do not join their new syntheses (fails safe) | kernel engine stamped reads, then the EpiScience follow-up |
| Recall audit rows | the kernel's pool-based recall entry point writes an instance-wide audit row carrying the query text and the returned claim ids | the same follow-up (stage 1 on the connection-scoped recall) |
| Suspended-client jobs | jobs already queued by a since-suspended OAuth client run until the job age cap (24 hours) | the age cap |
| Agents with their own OAuth client | such agents act in their own groups, not their operator's | kernel parity (kernel question) |
| Seeds from another of the owner's groups (until the worker split) | the in-process worker seeds a synthesis with every claim its owner can read; the claim-attach rule (the guard, and the membership repository before the guard exists) refuses a membership set citing a group claim owned by a group other than the synthesis', so such a synthesis fails at stage 2 (fail closed, nothing leaks) | the worker's seed filter (public claims plus claims of the synthesis' own group) |
| Events of group syntheses | `synthesis.*` events are published for publishable (public) syntheses only; a group synthesis emits none | by design (the kernel events table has no row security) |
| Deferred PROV edges | a group synthesis' outbox rows are deferred (`private`); after it is widened, its kernel edges are written by the next reconcile (server restart until the worker split) | the worker's worklist |
| Content-dedup existence oracle | the kernel deduplicates claims by content across owners, so an observation whose text equals another group's non-public claim is refused (nothing linked, no id returned), which tells the caller that a non-public claim with exactly that content exists | kernel (owner-scoped content dedup) |
| Audit rows the reverse trusts | the backfill reverse trusts `episcience.maint.backfill_owners` audit rows; the narrow maintenance login cannot write them, but an application-role login can write `episcience.`-prefixed audit rows until the kernel restricts the prefix | the kernel's `episcience.` audit-prefix restriction |
| Signer key kind on the application role | the countersign signer lookup reads `agents.key_kind`; contract item C13 lists column SELECT on `agents(id, public_key, display_name)` only | add `key_kind` to C13 before the application-role switch |
| Deploy-window completions | between the expand step and the contract step, a public synthesis that takes a non-public member of its OWN group (a member of another group is refused, as after the contract step) is not narrowed until 5035's data step runs (minutes; its kernel edges and events are still withheld by stage 6's publishability check) | the contract step |
| Rollback to the pre-ownership binary | that binary reads samples, protocols and blobs with no ownership filter (and countersignatures by claim), so a row written as `group` in one of those tables becomes readable by every token holder after a rollback; `docs/runbooks/e1c-rollback-vocabulary.sql` prints the per-table count first, for the operator to decide on before starting that binary | operator decision at rollback time |
| Contract test gap | C1 (a missing kernel role) is not exercised by a test: the kernel roles are cluster-scoped and shared with other workloads, and dropping or renaming one would break them. It is asserted by 5033 and the boot probe | review |
