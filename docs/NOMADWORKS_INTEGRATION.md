# NomadWorks / CodeNomad / OpenCode → Rust playground

How to run StarWorld’s agentic methodology on `playground/rust` (custom Claude Code + Ollama).

## Verdict

**Skills port is mostly wiring, not rewriting.** OpenCode/NomadWorks skills already use Agent Skills `SKILL.md` — the same format `playground/rust` loads from `.claude/skills/`.

**Agents, NomadWorks plugin tools, and the task queue are the real integration work.** CodeNomad should become a cockpit that *spawns* the Rust agent loop, not a second inference runtime.

## Source of truth (do not fork)

| Asset | Path |
| --- | --- |
| Skills | `/Users/alexshapiro/starworld/.opencode/skills/` (~60) |
| Slash commands | `/Users/alexshapiro/starworld/.opencode/command/` |
| NomadWorks agents | `/Users/alexshapiro/starworld/.nomadworks/agents/` (~46) |
| Policies | `/Users/alexshapiro/starworld/.nomadworks/policies/` |
| Tasks | `/Users/alexshapiro/starworld/tasks/` |
| CodeNomad | `/Users/alexshapiro/starworld/CodeNomad/` |
| OpenCode config | `/Users/Shared/opencode/config/opencode.json` |

This course repo keeps **adapters + symlinks + docs** only.

## Target runtime

```
CodeNomad / NomadWorks UI
        │  spawn session
        ▼
playground/rust  (−p prompt, cwd = target repo)
        │  tools: Read, Write, Bash, Skill, [NomadWorks…]
        │  skills: .claude ← .opencode ← .agents
        ▼
Ollama  (ollama.tokenizin.com/v1 → 127.0.0.1:11434)
```

## Phases

### P0 — Skill discovery ✅

Implemented in `playground/rust` and `playground/python`:

1. Multi-root scan (walk up from cwd): `.claude/skills` → `.opencode/skills` → `.agents/skills`
2. Extra roots via `NOMAD_SKILLS_PATH` / `OPENCODE_SKILLS_PATH` / `SKILLS_EXTRA_PATH` (colon-separated)
3. Precedence: closer ancestors win; within a level `.claude` > `.opencode` > `.agents`; first-wins on name with stderr warning
4. Debug: `SKILLS_DEBUG=1`
5. Scope: default stops at the git root (`SKILLS_SCOPE=all` also walks into `~/.claude`, `~/.opencode`, …)

**Link P1 skills into this repo** (symlinks only — StarWorld remains SoT):

```bash
./scripts/link-nomad-skills.sh
# STARWORLD=/path/to/starworld ./scripts/link-nomad-skills.sh
# ./scripts/link-nomad-skills.sh --dry-run
# ./scripts/link-nomad-skills.sh --unlink
```

### P1 — Methodology skills (symlink list)

Linked by `scripts/link-nomad-skills.sh`:

- `task-management`
- `project-delivery-management`
- `project-state`
- `ecosystem-readiness`
- `qa-validation-pipeline`
- `context-reconcile`
- `codenomad-build-restart` (from `CodeNomad/.opencode/skills`)
- `codenomad-architecture-guide` (from `CodeNomad/.opencode/skills`)
- `sidecar-management`
- `set-agent-model`

Run the Rust binary with **cwd = StarWorld** (or the target app) so `tasks/` and skill `scripts/` resolve — or rely on symlinks from this repo’s `.claude/skills/`.

### P2 — Agent roles via `context: fork` ✅

Implemented in `playground/rust` (`agents.rs`, `opencode_config.rs`) and `playground/python` (`agents.py`, `opencode_config.py`).

**Default (`NOMAD_AGENTS=opencode`):** load the CodeNomad / StarWorld OpenCode agent catalog (`OPENCODE_CONFIG`, usually `CodeNomad/.opencode/opencode.jsonc` — ~68 primary/subagents). Prompt files (`{file:…}`) resolve to `.nomadworks/agents/*.md` or `.opencode/agents/*.md`. Provider models are remapped to Ollama via `playground/opencode-rust.json`.

| Mode (`NOMAD_AGENTS`) | Behavior |
| --- | --- |
| `opencode` (default) | Full OpenCode/CodeNomad `agent` map as fork skills + slash aliases (`pma`, `ba`, …) |
| `core` | P2 delivery roles only (`product_manager`, `ba`, `tl`, `dev`, `qa`, `runner`) |
| `all` | OpenCode catalog + any extra `.nomadworks/agents/*.md` not already listed |
| `off` | No auto-registered agent forks |

| Alias | Canonical |
| --- | --- |
| `/pma` | `product_manager` |
| `/ba` | `business_analyst` |
| `/tl`, `/tech-lead` | `tech_lead` |
| `/dev` | `developer` |
| `/qa` | `qa_engineer` |
| `/runner`, `/wr` | `workflow_runner` |

Also available when catalog loads: `/ceo`, `/cto`, `/opencoder`, `/copywriter`, `/codenomad_agent_runner`, … (every `agent` key in the JSON).

Fork path injects policy markdown from `.nomadworks/policies/` (`NOMAD_POLICY_MAX_CHARS`, default 14000). Per-agent models use OpenCode pins remapped to Ollama (default fork model `qwen3.6:latest`). Disk skills still win on name collision.

### P3 — NomadWorks tools ✅

Registered on `ToolMode::Base` and `All` (forks included) in Rust (`nomad_tools.rs`) and Python (`nomad_tools.py`):

| Tool | Playground behavior |
| --- | --- |
| `nomadworks_validate` | `mode=phase0` (default) → `scripts/validate-nomadworks.sh`; `mode=strict` → `@neuralnomads/nomadworks` CodeMap walk (truncated) |
| `nomadworks_start_discussion` / `stop_discussion` | File + `.codenomad/runtime/discussions.json` shim (`NOMAD_SESSION_ID` or `playground-{pid}`); no OpenCode live capture |
| `nomadflow_run_workflow` | **Synchronous** fork via `workflow_runner` (`/runner`) — not an OpenCode background session |
| `nomadflow_prompt_workflow` | Returns guidance to call `nomadflow_run_workflow` again |

Also registers `workflow_runner` (+ `/runner`, `/wr`) as a P2-style forked agent.

### P4 — CodeNomad bridge ✅

**Course repo CLI** (primary adapter):

```bash
./scripts/codenomad-rust-bridge.sh --prompt "/smoke-echo" --cwd "$PWD" --jsonl
./scripts/codenomad-rust-bridge.sh --agent pma --prompt "In one sentence: what is your role?" --cwd /Users/alexshapiro/starworld --jsonl
```

Emits NDJSON: `meta` → `stderr`/`stdout` lines → `final` → `exit`. Set `CLAUDE_CODE_PLAYGROUND` if the script is invoked outside this repo.

**CodeNomad HTTP** (streams the same bridge):

| Method | Path | Body |
| --- | --- | --- |
| `GET` | `/api/playground/rust/status` | — |
| `POST` | `/api/playground/rust/run` | `{ "prompt", "cwd"?, "agent"?, "jsonl"?, "persist"? }` → NDJSON stream |

Route: `CodeNomad/packages/server/src/server/routes/playground-rust.ts` (registered in `http-server.ts`). Requires CodeNomad restart to pick up.

**CodeNomad UI (chat engine switch):** Settings → **Claude Code (Rust)** → enable **Use Rust as chat engine**. Chat sends then call the bridge (session agent / default agent from OpenCode catalog). Optional persist to Neon + MinIO. Server env:

```bash
export CLAUDE_CODE_PLAYGROUND=/Users/alexshapiro/0-claudecode/build-your-own-claude-code
export STARWORLD=/Users/alexshapiro/starworld
export OPENCODE_CONFIG=$STARWORLD/CodeNomad/.opencode/opencode.jsonc
export NOMAD_AGENTS=opencode
# optional: PLAYGROUND_PERSIST=1
```

## What not to do

- Do **not** copy 60 skill trees into this Codecrafters course repo as a second SoT.
- Do **not** expect OpenCode plugin APIs inside Rust without an explicit tool shim.
- Do **not** point OpenCode’s primary provider at the public tunnel if localhost works on the Studio; the playground already falls back.

## Smoke

```bash
# From course repo
./scripts/link-nomad-skills.sh
cd playground/rust
SKILLS_DEBUG=1 ./your_program.sh -p "/smoke-echo"

# Or point at StarWorld skills without symlinks
NOMAD_SKILLS_PATH=/Users/alexshapiro/starworld/.opencode/skills \
  ./your_program.sh -p "/task-management status"

# P2 — forked NomadWorks role (needs STARWORLD or NOMADWORKS_PATH in playground/.env)
SKILLS_DEBUG=1 ./your_program.sh -p "/pma In one sentence: what is your role?"

# P3 — validate (model should call nomadworks_validate)
./your_program.sh -p "Call nomadworks_validate with mode phase0 and quote the PASS/FAIL line."

# P4 — CodeNomad bridge (from course repo root)
./scripts/codenomad-rust-bridge.sh --jsonl --prompt "/smoke-echo"
./scripts/codenomad-rust-bridge.sh --persist --jsonl --prompt "/smoke-echo"
# After CodeNomad restart:
# curl -sN http://127.0.0.1:9940/api/playground/rust/status
# curl -sN http://127.0.0.1:9940/api/playground/rust/run \
#   -H 'content-type: application/json' \
#   -d '{"prompt":"/smoke-echo","jsonl":true}'
```

**Integration phases P0–P4 complete** for the Rust playground path. Evidence: `evidences/playground-p0-p4/`. Persistence: `playground/persistence/` (ZenStack V3 → local Neon + MinIO). Optional later: richer JSON event protocol (`tool_call` / `tool_result`) from the Rust binary itself.
