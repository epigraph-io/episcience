# episcience

An Apache-2.0 layer over [EpiGraph](https://github.com/epigraph-io/epigraph) that adds the experimental loop: samples, protocols, blobs, countersignatures, and synthesis claims. Where EpiGraph models *what is believed*, episcience models *how beliefs were tested* — the scaffolding needed to do science (or any methodologically rigorous knowledge work) on top of the kernel.

The synthesis pipeline ships with a pluggable [skill foundation](docs/intro/02-concepts-science.md#7--synthesis-skills) (`baseline` and `lab_notebook`), a [verifier-driven](docs/intro/02-concepts-science.md#8--verifier-driven-acceptance) acceptance gate, a [novelty score](docs/intro/02-concepts-science.md#9--novelty-assessment) per accepted synthesis, simulated-annealing [refinement chains](docs/intro/02-concepts-science.md#10--refinement-chains) on verifier reject, an [MCP write surface](docs/intro/01-quickstart-extension.md#step-5--register-the-mcp-server-with-claude-code) at parity with the HTTP routes, and a [structured section vocabulary](docs/intro/02-concepts-science.md#11--protocol-section-vocabulary) on protocols.

## Status

- Version: 0.1.0
- License: Apache-2.0
- Maturity: alpha; the workspace pins specific epigraph crates by git rev in [`Cargo.toml`](Cargo.toml). The pin is currently kept on the head of `feat/phase0-integrated` (PR epigraph-io/epigraph#10); will re-pin to a merged sha once #10 lands.

## Prerequisites

A running EpiGraph kernel — same Postgres instance is fine. Start there: https://github.com/epigraph-io/epigraph#5-minute-quickstart.

## 5-minute extension quickstart

```bash
# 1. Clone
git clone https://github.com/epigraph-io/episcience.git && cd episcience

# 2. Apply episcience migrations on the kernel DB (run per-file via psql; see
#    docs/intro/01-quickstart-extension.md for the full sequence and why we
#    don't use `sqlx migrate run` here — the 001 version collides with the
#    kernel's _sqlx_migrations entry).
for f in migrations/001_initial_schema.sql migrations/5*.sql migrations/synthesis/5*.sql; do
  psql postgres://epigraph:epigraph@localhost/epigraph -f "$f"
done

# 3. Build and start (port 8091 to avoid colliding with epigraph-api on 8080
#    and with EPIGRAPH_API_URL's default of 8090)
cargo build --release -p episcience-api
EPISCIENCE_PORT=8091 \
  EPIGRAPH_API_URL=http://127.0.0.1:8080 \
  DATABASE_URL=postgres://epigraph:epigraph@localhost/epigraph \
  cargo run --release -p episcience-api --bin episcience-server &

# 4. Register the MCP server in ~/.mcp.json alongside the epigraph entry
# (see docs/intro/01-quickstart-extension.md for the JSON block)

# 5. In Claude Code, call mcp__episcience__synthesize with query "test" and
#    wait_for_completion true; then mcp__episcience__recall_synthesis with
#    query "test" to read it back.
```

## Onboarding tree

- [`docs/intro/01-quickstart-extension.md`](docs/intro/01-quickstart-extension.md) — five-step setup assuming kernel installed
- [`docs/intro/02-concepts-science.md`](docs/intro/02-concepts-science.md) — experiments, samples, protocols, blobs, countersigning, synthesis, PROV-O, skills, verifier, novelty, refinement, protocol sections
- [`docs/intro/03-walkthroughs.md`](docs/intro/03-walkthroughs.md) — three end-to-end transcripts (coming soon — captured live)
- [`docs/intro/04-glossary.md`](docs/intro/04-glossary.md) — science-specific terms (kernel terms link to the EpiGraph glossary)
- [`docs/intro/05-workflows.md`](docs/intro/05-workflows.md) — three end-to-end walkthroughs: default synthesis with verifier accept, refinement on reject, ELN turn through MCP

## Why a separate repo?

EpiGraph is the public-Apache-2.0 epistemic kernel that other applications (some open, some closed) can depend on. Science-specific scaffolding is its own bounded concern, and lives in its own repo so it can evolve at its own pace and so consumers who don't need the science layer aren't forced to take it.

## Deeper EpiGraph material

- [EpiGraph next-steps](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/05-next-steps.md) — contributor, deploy, downstream pointers
- [EpiGraph concepts](https://github.com/epigraph-io/epigraph/blob/main/docs/intro/02-concepts.md) — kernel mental model
