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
- `legacy/` — the hand-applied history (`001_initial_schema.sql`,
  `5000`-`5026`, `synthesis/5011`-`5032`). Kept for reference; run by nothing.
  sqlx's resolver reads only the top level of this directory.

## Version ranges

- `5032` — the legacy baseline.
- `5033` and up — the E1 tenancy series (contract, ownership columns, RLS,
  definers, cleanup). New migrations take the next free number.
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
```

`episcience-migrate` reads only `EPISCIENCE_MIGRATION_DATABASE_URL` and refuses
to start while `DATABASE_URL` is set. The kernel schema is never built from this
repository: tests and CI run the kernel's own `epigraph-migrate` at the pinned
rev (`scripts/e1-test-db.sh`).
