---
name: course-map
description: Use when navigating the Build your own Claude Code course repo, finding stage specs, or implementing tools and skills for the challenge.
---

# Course map (Build your own Claude Code)

This repository is **course content** for [CodeCrafters Claude Code](https://app.codecrafters.io/courses/claude-code/overview), not the learner submission repo.

## Where things live

| Area | Path |
| --- | --- |
| Course stages (marketing + slugs) | `course-definition.yml` |
| Stage instructions (authoritative prose) | `stage_descriptions/base-*.md`, `stage_descriptions/skills-*.md` |
| Skills extension design notes | `extension_designs/skills.md` |
| Starters learners receive | `starter_templates/`, `compiled_starters/` |
| Reference solutions (partial) | `solutions/` (stages 1–2 only today) |
| **Full local reference (Rust, preferred)** | `playground/rust/` |
| **Full local reference (Python)** | `playground/python/` |

## Base stages (tools)

1. `yy2` — LLM API
2. `aq1` — advertise `Read`
3. `md6` — execute `Read`
4. `ff2` — agent loop
5. `oz7` — `Write`
6. `oq5` — `Bash`

## Skills extension

1. `vh1` — catalog in system prompt
2. `jd8` — `/name` invocation
3. `wd2` — `$ARGUMENTS`, `$0`, `$1`
4. `sk5` — stack `/a /b args`
5. `tq1` — bundled `scripts/`
6. `gq2` — `Skill` tool
7. `mj2` — `context: fork` subagent

## Local run (reference)

From repo root (Rust preferred on Apple Silicon):

```bash
cd playground/rust
./your_program.sh -p "Hello"
# Inference: Ollama via ollama.tokenizin.com (fallback localhost:11434)
```

Skills are discovered by walking up from cwd across `.claude/skills`, `.opencode/skills`, and `.agents/skills`, plus `NOMAD_SKILLS_PATH`. Link NomadWorks P1 skills with `./scripts/link-nomad-skills.sh`.
