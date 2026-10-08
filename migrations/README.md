# EpiScience migrations

EpiScience's schema sits on top of the EpiGraph kernel's. The two schemas have
**separate ledgers**:

| Ledger | Written by | Holds |
|---|---|---|
| `public._sqlx_migrations` | the kernel's `epigraph-migrate` only | kernel versions (the pinned rev's set) |
| `episcience_meta._sqlx_migrations` | `episcience-migrate` only | EpiScience versions (5032 and up) |

The kernel's migrator refuses a database whose ledger holds a version it does
not embed, so an EpiScience version must never reach `public._sqlx_migrations`.
`episcience-migrate` connects with `search_path = episcience_meta`; sqlx names
its ledger table unqualified, so it lands in `episcience_meta`. For the same
reason every object an EpiScience migration creates or references is
`public.`-qualified.

## Layout

- `5032_legacy_baseline.sql` — the 14 EpiScience tables, consolidated (see its
  header for what it excludes and why). The first version of the ledger.
- `5032_legacy_baseline.fingerprint` — the canonical fingerprint of those
  tables. `episcience-migrate adopt-baseline` records 5032 on a legacy
  database only when its live tables match this file exactly.
- `5033_kernel_contract_v1.sql` — tenancy contract v1
  (`docs/tenancy-contract.md`): asserts the kernel objects EpiScience relies
  on, creates `public.episcience_assert_kernel_contract(int)`,
  `public.episcience_session_is_privileged()` and the NOLOGIN roles
  `episcience_rw`, `episcience_queue`, `episcience_maint_ops`.
- `5034_tenancy_columns_expand.sql` — tenancy columns, EXPAND step: the
  nullable ownership pair (`owner_group_id`, `visibility`) on the 12 tenancy
  tables, `synthesis_jobs.principal_id`, `syntheses.staleness_checked_at`,
  `synthesis_provo_edges.deferred_reason`,
  `countersignatures.countersigned_by`, the kernel maintenance role's table
  privileges, and the two one-shot maintenance-owned definers
  `episcience_maint_backfill_owners(principal, apply)` /
  `episcience_maint_backfill_reverse(manifest)` (EXECUTE: `episcience_maint_ops`
  only). Nothing is enforced yet.
- `5035_tenancy_columns_contract.sql` — tenancy columns, CONTRACT step: legacy
  `private`/`shared` become `group`; derived rows take their parent's pair;
  an ownerless root refuses the migration (run the re-own first); the pair
  becomes mandatory (`public`/`group`, no world or seed owner); the row guards
  (all SECURITY INVOKER): `tenancy_10_require` / `_inherit`,
  `tenancy_15_author`, `tenancy_20_claim_guard` / `_principal`,
  `tenancy_30_owner_immutable` / `_derived_pinned`,
  `tenancy_40_widening_guard` (interlock `episcience.allow_widen` + every
  input public), `tenancy_45_publish_rule`; and the one DEFINER,
  `tenancy_90_propagate` (a parent's pair reaches every child). Undo:
  `docs/runbooks/5035-undo.sql` (compensating, never a migration).
- `5036_row_security.sql`, `5037_queue_and_maintenance_definers.sql` — row
  security, the policies and the grant matrix; the queue, worklist, chain-head
  and sweep definers (`docs/tenancy-contract.md`). Undo:
  `docs/runbooks/e1e-undo.sql`.
- `5038_countersignature_hash_guard.sql` — the insert-time refusal of a
  countersignature without its link hash from a non-privileged session (the
  worker split's first application login can write that table).
- `5039_sweep_blocked_detector.sql` — `episcience_maint_unpublishable_public()`,
  the definer `episcience-maint tick` alerts from (the rows the narrowing sweep
  could not narrow). Undo of both: `docs/runbooks/e1f-undo.sql`.
- `5040_detach_shared_evidence_trigger.sql` — drops the legacy
  `edges_shared_evidence` trigger on the kernel's `edges` table and its
  function `create_shared_evidence_factor()` (created by the hand-applied
  `001_initial_schema.sql`, never by the kernel); a no-op on a database built
  from the baseline, which never had them. The migration lint's only
  kernel-object allowlist. Undo (operator request only):
  `docs/runbooks/5040-undo.sql`.
- `5041_syntheses_skill_wiki_article.sql` — widens the
  `syntheses_skill_name_known` CHECK to the `wiki_article` skill (wiki Phase
  B). Every registered skill must be accepted by this CHECK:
  `crates/episcience-db/tests/synthesis_repo_test.rs` loops over
  `episcience_core::synthesis::skills::registered_names()` and fails otherwise,
  so a new skill ships with its own widening migration. Undo (operator
  request only; the step before `5040-undo.sql`):
  `docs/runbooks/5041-undo.sql`.
- `5042_wiki_article_columns.sql` — `syntheses.seed_theme_id` (the theme a
  wiki article was seeded from; provenance only, no FK) and
  `syntheses.wiki_key` (the page key, `episcience_core::wiki::WikiKey::as_slug`),
  their shape and pair CHECKs, and the partial page index the wiki registry
  reads (wiki Phase B). Undo (operator request only; drops the page keys; the
  step before `5041-undo.sql`): `docs/runbooks/5042-undo.sql`.
- `legacy/` — the hand-applied history (`001_initial_schema.sql`,
  `5000`-`5026`, `synthesis/5011`-`5032`). Kept for reference; run by nothing.
  sqlx's resolver reads only the top level of this directory.

## Version ranges

- `5032` — the legacy baseline.
- `5033` and up — the E1 tenancy series (contract, ownership columns, RLS,
  definers, cleanup). New migrations take the next free number.

## Rules for every migration from 5033 on

Checked by `crates/episcience-db/tests/migration_lint.rs` (no database):

- the first statement is `SELECT public.episcience_assert_kernel_contract(1);`
  (5033 itself opens with the same checks inline);
- no session `search_path` change; every function is created with
  `SET search_path = public, pg_temp`;
- every object is `public.`-qualified (the migrator's `search_path` is
  `episcience_meta`); function bodies may use unqualified names (their
  `search_path` is pinned);
- no DDL, DML or grant on a kernel table, and only `public.episcience_*`
  functions (one allowlisted detach excepted), in top-level statements, DO
  blocks, function bodies and dynamic SQL; object lists are read in full; a
  function body may INSERT into `public.security_events` (audit rows) and
  nothing else on a kernel table; an index may be altered or dropped only if a
  migration here created it;
- no role DDL, membership grant/revoke or role switch (5033's NOLOGIN roles
  excepted; `set_config('role' | 'session_authorization', …)` included, also
  inside dynamic SQL, and `set_config` always names its setting with a plain
  literal), and no schema-, database- or cluster-level statement;
- `EXECUTE` runs only a literal, or a `format()` literal using `%I` / `%L`
  only, followed by nothing but the end of the statement, `INTO` or `USING`:
  never a variable, a concatenation (also after the `format(…)` call) or a
  second literal. Write one explicit statement per table rather than a loop
  over names: `format('ALTER TABLE public.%I …', t)` is read as a write to
  `public.%I` and refused;
- no string literal is continued by an adjacent literal (`'a'` newline `'b'`,
  which SQL joins into one string), anywhere;
- names are read with whitespace around the schema dot removed, and `UPDATE`
  with its whole grammar (`ONLY`, `*`, a bare or quoted alias);
- every kernel `epigraph_*` name is a contract-v1 name, except an
  `epigraph_`-prefixed column a migration here declared on an EpiScience table
  (such as `synthesis_provo_edges.epigraph_edge_id`), used as a column (not
  called, not as a role); no kernel `epigraph.*` setting; none of the listed
  excluded kernel objects;
- the kernel ledger is never written; no `ON ALL … IN SCHEMA`, no
  `ALTER DEFAULT PRIVILEGES`; no uuid literal other than the world and seed
  sentinels.
- from 5036 on, the second statement is `SET LOCAL lock_timeout = '<n>s'`
  (these migrations lock tables the running service uses; a busy table makes
  the migration give up, nothing applied, instead of queueing the service);
- a migration that creates a table repeats 5036's REVOKE for it (the
  kernel's default privileges give every new `public` table to the kernel
  application role) and adds it to the tenancy model; ratchets R1
  (`tenancy_coverage.rs`) and R4 (`privilege_matrix.rs`) fail otherwise.
- `5027` is permanently vacant: a top-level 5027 would sort before the
  consolidated baseline that creates the tables it would touch, and below the
  legacy 5028-5032 already applied by hand on legacy databases.

## Running

```sh
# Fresh database whose kernel schema was built by `epigraph-migrate`:
EPISCIENCE_MIGRATION_DATABASE_URL=... episcience-migrate run

# An expand step before its data step: apply up to one version only.
EPISCIENCE_MIGRATION_DATABASE_URL=... episcience-migrate run --to 5034

# Legacy database (tables built by the hand-applied files): record 5032 without running it.
EPISCIENCE_MIGRATION_DATABASE_URL=... episcience-migrate adopt-baseline

episcience-migrate status

# After run: ledger complete and consistent, kernel ledger isolated, tenancy
# contract v1 holds, and (from 5036) the tenancy catalog matches the model:
# the closed definer set, row security enabled and forced, the exact grant
# matrix, the exact policy set and its shapes, the principal guard on every
# tenancy table, no sentinel-owned row. Non-zero exit = do not deploy.
episcience-migrate verify
```

An applied (or adopted) version is frozen: sqlx records the checksum of the file
it ran or adopted, and a later `run` refuses a database whose recorded checksum
no longer matches the embedded file. Never edit a migration that has been
recorded anywhere; change the schema with a new version.

`episcience-migrate` reads only `EPISCIENCE_MIGRATION_DATABASE_URL` and refuses
to start while `DATABASE_URL` is set. The kernel schema is never built from this
repository: tests and CI run the kernel's own `epigraph-migrate` at the pinned
rev (`scripts/e1-test-db.sh`).
