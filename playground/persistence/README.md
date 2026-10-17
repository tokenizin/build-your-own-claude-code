# Playground persistence (ZenStack V3)

Stores tool-run transcripts and redacted config for the Rust/Python playground and CodeNomad bridge.

| Store | Container / endpoint | Purpose |
| --- | --- | --- |
| PostgreSQL | `neondb-local` via `127.0.0.1:54329` | `PlaygroundConfig`, `ToolRun`, `ToolEvent`, `BlobObject` |
| MinIO | `blob-db-local` → `127.0.0.1:9000` | NDJSON transcripts (`s3://claude-code-playground/…`) |

Schema: `zenstack/schema.zmodel` (ZenStack **V3** — `@zenstackhq/orm`, **no Prisma client**). Access policies use `@zenstackhq/plugin-policy`.

## Setup

```bash
# Requires: docker (neondb-local, blob-db-local), bun
cd playground/persistence
cp .env.example .env   # if needed
bun install
bun run setup          # pg proxy + zen generate + db push + MinIO bucket
bun run smoke          # writes one ToolRun + blob
```

Host Postgres often occupies `:5432`. `scripts/ensure-pg-proxy.sh` publishes Neon on **54329**.

## Record a bridge run

```bash
# from repo root (not this directory)
cd ../..   # → build-your-own-claude-code/
./scripts/codenomad-rust-bridge.sh --persist --jsonl --prompt "/smoke-echo"
# or PLAYGROUND_PERSIST=1 ./scripts/codenomad-rust-bridge.sh --jsonl -p "…"
```

The bridge pipes NDJSON into `bun run src/record-cli.ts`.

## Models

- **PlaygroundConfig** — named config snapshot (secrets redacted)
- **ToolRun** — one agent/bridge invocation
- **ToolEvent** — ordered NDJSON events
- **BlobObject** — MinIO pointer (sha256, uri, label)
