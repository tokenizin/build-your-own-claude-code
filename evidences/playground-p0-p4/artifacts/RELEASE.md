# Release — playground-rust P0–P4

**Tag (suggested):** `playground-rust-p0-p4-2026-10-13`  
**Binary:** host-optimized release build of `codecrafters-claude-code`  
**Checksum:** see `SHA256SUMS` in this directory

## Install / run

```bash
cd playground/rust
./your_program.sh -p "/smoke-echo"
# or
./scripts/codenomad-rust-bridge.sh --jsonl --prompt "/smoke-echo"
```

Requires `playground/.env` (see `.env.example`). Optional: `STARWORLD`, `CLAUDE_CODE_PLAYGROUND`.

## CodeNomad

Restart CodeNomad server to load `POST /api/playground/rust/run`.

## Persistence (local Neon + MinIO)

```bash
# Prerequisites: docker containers neondb-local, blob-db-local
cd playground/persistence
cp .env.example .env   # if needed
bun install
bun run setup          # socat :54329 + zen generate + db push + bucket
bun run smoke

# Record a live bridge run
cd ../..
./scripts/codenomad-rust-bridge.sh --persist --jsonl --prompt "/smoke-echo"
```

Schema: ZenStack V3 `schema.zmodel` → `@zenstackhq/orm` (not Prisma). Details: `playground/persistence/README.md`.
