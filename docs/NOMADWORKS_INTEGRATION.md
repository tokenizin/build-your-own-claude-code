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

Implemented in `playground/rust` (`agents.rs`) and `playground/python` (`agents.py`):

1. Discover `.nomadworks` via walk-up from cwd, `NOMADWORKS_PATH`, or `STARWORLD/.nomadworks`
2. Register **core** roles only as `context: fork` skills (avoids flooding the catalog with ~46 agents):

| NomadWorks agent | Slash aliases | Default model |
| --- | --- | --- |
| `product_manager` | `/pma` | `qwen3.6:latest` |
| `business_analyst` | `/ba` | `qwen3.6:latest` |
| `tech_lead` | `/tech-lead`, `/tl` | `qwen3.6:latest` |
| `developer` | `/dev` | `qwen3.6:latest` |
| `qa_engineer` | `/qa` | `qwen3.6:latest` |

3. Fork path injects policy markdown from `.nomadworks/policies/` as system messages (`NOMAD_POLICY_MAX_CHARS`, default 14000)
4. Per-agent `model` frontmatter (or defaults above) overrides `OLLAMA_MODEL` for that nested loop only
5. Slash `/pma …` and `Skill` tool both use the same fork + policy path

Disk skills still win on name collision over auto-registered agents.

### P3 — NomadWorks tools (3–5 days)

Register alongside existing tools (or Bash wrappers calling the NomadWorks CLI/plugin):

- `nomadworks_validate`
- `nomadworks_start_discussion` / `nomadworks_stop_discussion`
- `nomadflow_run_workflow` / `nomadflow_prompt_workflow`

Until native handlers exist, skills that shell out via `Bash` can bridge the gap for P1.

### P4 — CodeNomad bridge (2–4 days)

CodeNomad session adapter:

1. Resolve target project cwd + prompt (+ optional agent/skill).
2. `exec` `playground/rust/your_program.sh -p "…"` with env from `playground/.env`.
3. Stream stderr/stdout into the CodeNomad transcript.
4. Optional: JSON event line protocol later (tool_call / tool_result / final).

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
```

**Next:** P3 — NomadWorks tools.
