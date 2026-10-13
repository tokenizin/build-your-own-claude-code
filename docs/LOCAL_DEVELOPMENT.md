# Local development — full tools and skills

The CodeCrafters challenge teaches features **stage by stage**. This repo also includes a **reference build** so you can run and debug the complete agent (all base tools + Skills extension) without implementing every stage first.

## Prerequisites

- **Preferred (Mac Studio Ultra):** [Rust](https://rustup.rs/) (`cargo`) — `playground/rust/`
- Also available: [uv](https://docs.astral.sh/uv/) — `playground/python/`
- Local [Ollama](https://ollama.com/) on the Mac Studio (tunnel: `https://ollama.tokenizin.com`)

Inference uses **Ollama on the Mac Studio** via the Cloudflare tunnel (OpenAI-compatible `/v1`), with a localhost fallback. Put config in `playground/.env` (gitignored); both `your_program.sh` runners load it automatically:

```bash
# playground/.env
OLLAMA_BASE_URL=https://ollama.tokenizin.com/v1
OLLAMA_FALLBACK_BASE_URL=http://127.0.0.1:11434/v1
OLLAMA_API_KEY=ollama
OLLAMA_MODEL=gemma4:31b-cloud
OLLAMA_MAX_TOKENS=2048
```

If the public tunnel is blocked (HTTP 403), the playground auto-falls back to `127.0.0.1:11434` (same daemon / `*:cloud` models). See `playground/.env.example`.

**Tunnel 403 note:** Ollama rejects non-local `Host` / `Origin`. The StarWorld tunnel rule for `ollama.tokenizin.com` must set `originRequest.httpHostHeader` to `127.0.0.1:11434` (`starworld/scripts/sync-tunnel-ingress.sh`). Without that, edge returns empty 403 even when localhost Ollama is healthy.

## Run the reference agent

**Rust (recommended on Apple Silicon):**

```bash
cd playground/rust
./your_program.sh -p "How many tools are in this request? Number only."
```

Release builds use LTO and `RUSTFLAGS=-C target-cpu=native` for the host CPU.

**Python (same behavior):**

```bash
cd playground/python
./your_program.sh -p "How many tools are in this request? Number only."
```

### What is implemented

| Feature | Stage slug | Notes |
| --- | --- | --- |
| OpenRouter chat | `yy2` | `-p` prompt |
| `Read` | `aq1`–`md6` | |
| Agent loop | `ff2` | multi-step tool use |
| `Write` | `oz7` | |
| `Bash` | `oq5` | |
| Skill catalog | `vh1` | system prompt |
| `/skill` invoke | `jd8` | |
| Arguments | `wd2` | `$ARGUMENTS`, `$N` |
| Stacking | `sk5` | `/a /b trailing` |
| Script skills | `tq1` | location prefix when body mentions `scripts/` |
| `Skill` tool | `gq2` | |
| Subagent fork | `mj2` | `context: fork` |

Sources:

- Rust: `playground/rust/src/main.rs`, `skills.rs`, `agents.rs`
- Python: `playground/python/app/main.py`, `skills.py`, `agents.py`

## Project skills (Claude Code + reference)

Discovery (multi-root, walk-up from cwd):

1. `.claude/skills`
2. `.opencode/skills`
3. `.agents/skills`
4. Extra: `NOMAD_SKILLS_PATH` / `OPENCODE_SKILLS_PATH` / `SKILLS_EXTRA_PATH`

Ship-local skills:

- `course-map` — where stage docs and starters live
- `smoke-echo` — quick `/smoke-echo` check

Link NomadWorks / OpenCode P1 skills (symlinks into `.claude/skills/`):

```bash
./scripts/link-nomad-skills.sh
```

In **Claude Code** (terminal), open this repository so project skills load automatically. `.claude/settings.json` pre-allows common tools for local work.

Quick checks:

```bash
# Slash invoke (reference program)
cd playground/rust
./your_program.sh -p "/smoke-echo"

# Model-picked skill (needs API key)
./your_program.sh -p "Please run the smoke echo test."
```

## Working on the course vs the playground

- **Stage instructions**: edit `stage_descriptions/` and regenerate via [course-sdk](https://github.com/codecrafters-io/course-sdk) workflows.
- **Learner starters**: `starter_templates/` → `compiled_starters/`.
- **Reference behavior**: keep `playground/rust/` and `playground/python/` in sync when stage specs change.

Official solutions in `solutions/` currently cover early stages only; the playground is the full-feature local substitute until later solution folders exist.

## NomadWorks / CodeNomad / OpenCode

To run StarWorld methodology skills and agents on this Rust agent loop, see [NOMADWORKS_INTEGRATION.md](NOMADWORKS_INTEGRATION.md).

CodeNomad spawn bridge (P4):

```bash
./scripts/codenomad-rust-bridge.sh --jsonl --prompt "/smoke-echo"
# or POST http://127.0.0.1:9940/api/playground/rust/run  (after CodeNomad restart)
```

## Cursor

- Rule: `.cursor/rules/claude-code-course.mdc`
- Agent notes: `.cursorrules`
