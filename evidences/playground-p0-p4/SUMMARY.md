# Evidence — Playground P0–P4 (Rust path)

**Date:** 2026-10-13  
**Repo:** `build-your-own-claude-code`  
**Scope:** NomadWorks / CodeNomad integration phases P0–P4 on `playground/rust` + local persistence

## Verdict

| Gate | Result |
| --- | --- |
| `cargo test` (9) | PASS |
| `cargo build --release` | PASS |
| P0 multi-root skills + `/smoke-echo` | PASS → `papaya` |
| P2 `/pma` fork + model override | PASS → `e2e-p2` |
| P3 `nomadworks_validate` phase0 | PASS |
| P4 `codenomad-rust-bridge.sh --jsonl` | PASS → `final: papaya` |
| P4 bridge `--persist` → Neon + MinIO | PASS → `runId=cmv6gxf760000s0pl7hg2ev1r` |
| Ollama tunnel `ollama.tokenizin.com` | PASS HTTP 200 |
| Python parity (skills + validate) | PASS |
| ZenStack V3 `db push` + smoke | PASS |

Logs: `logs/e2e-suite.log`, `logs/python-parity.log`, `logs/persistence-smoke.log`, `logs/zen-*.log`, `logs/bridge-*.ndjson`.

## Phase map

| Phase | Deliverable |
| --- | --- |
| P0 | Multi-root skill discovery (`skills.rs`) |
| P1 | `scripts/link-nomad-skills.sh` |
| P2 | NomadWorks fork roles (`agents.rs`, `fork_user_body`) |
| P3 | NomadWorks tools (`nomad_tools.rs`) |
| P4 | CodeNomad bridge (`scripts/codenomad-rust-bridge.*` + CodeNomad route) |

## Persistence (enabled)

| Component | Detail |
| --- | --- |
| Schema | `playground/persistence/zenstack/schema.zmodel` (ZenStack V3, no Prisma client) |
| ORM | `@zenstackhq/orm` + `@zenstackhq/plugin-policy` |
| Postgres | `neondb-local` via `127.0.0.1:54329` (`claude_code_playground`) |
| Blob | MinIO `blob-db-local` bucket `claude-code-playground` |
| Bridge | `--persist` / `PLAYGROUND_PERSIST=1` → `record-cli.ts` |

Tables: `PlaygroundConfig`, `ToolRun`, `ToolEvent`, `BlobObject`.

## Release artifact

See `artifacts/` (release binary + `SHA256SUMS` + `RELEASE.md`).

```
sha256:27765c5206ca7b20b437d299970952669ff2c49f1675ea89a374694420e32998  codecrafters-claude-code
```
