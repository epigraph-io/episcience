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
- `5027` is permanently vacant: a top-level 5027 would sort before the
  consolidated baseline that creates the tables it would touch, and below the
  legacy 5028-5032 already applied by hand on legacy databases.

## Running

```sh
# Fresh database whose kernel schema was built by `epigraph-migrate`:
EPISCIENCE_MIGRATION_DATABASE_URL=... episcience-migrate run

# Legacy database (tables built by the hand-applied files): record 5032 without running it.
EPISCIENCE_MIGRATION_DATABASE_URL=... episcience-migrate adopt-baseline

episcience-migrate status

# After run: ledger complete and consistent, kernel ledger isolated, tenancy
# contract v1 holds. Non-zero exit = do not deploy.
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
