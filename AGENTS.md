## Learned User Preferences

- Prefer Rust over other course languages for the local full-feature playground on Mac Studio Ultra (Apple Silicon).
- Prefer Ollama on the Mac Studio (`ollama.tokenizin.com`, with localhost fallback) for inference instead of OpenRouter.
- Want CodeNomad / NomadWorks / OpenCode skills, tasks, and methodology wired into the Rust playground so agentic flows run there, not as a second inference stack.
- Prefer phased NomadWorks integration (P0 skill discovery → P1 linked methodology skills → P2 fork roles → later tool shims / CodeNomad spawn).

## Learned Workspace Facts

- This repo is the Codecrafters “Build your own Claude Code” course definition (stages + starters); learner solutions live on CodeCrafters, not as a complete checkout here.
- Local full reference agents live in `playground/rust/` (preferred) and `playground/python/` (parity): Read, Write, Bash, Skill, agent loop, skills extension including `context: fork`.
- Both `your_program.sh` runners auto-load `playground/.env` (gitignored); inference is OpenAI-compatible Ollama (`OLLAMA_BASE_URL` / fallback / model).
- Skill discovery walks up from cwd across `.claude/skills`, `.opencode/skills`, and `.agents/skills` (plus optional `*_SKILLS_PATH` extras), first-wins on name collisions, and by default stops at the git root (`SKILLS_SCOPE=all` to include home trees).
- StarWorld (`/Users/alexshapiro/starworld`) is the source of truth for OpenCode skills, NomadWorks agents/policies, and tasks; this course repo keeps adapters, symlinks, and docs only.
- `./scripts/link-nomad-skills.sh` symlinks P1 methodology skills into `.claude/skills/` (symlinks gitignored); set `STARWORLD` when the StarWorld path differs.
- NomadWorks core roles (`/pma`, `/ba`, `/tl`, `/dev`, `/qa`) load as `context: fork` skills with policy injection and optional per-agent model overrides; slash args must be appended via `fork_user_body` when agent bodies lack `$ARGUMENTS`.
- Integration plan and phase status: `docs/NOMADWORKS_INTEGRATION.md`; local run matrix: `docs/LOCAL_DEVELOPMENT.md`.
