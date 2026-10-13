from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass
from pathlib import Path

RELATIVE_SKILL_ROOTS = (
    ".claude/skills",
    ".opencode/skills",
    ".agents/skills",
)

EXTRA_PATH_ENV_KEYS = (
    "NOMAD_SKILLS_PATH",
    "OPENCODE_SKILLS_PATH",
    "SKILLS_EXTRA_PATH",
)


@dataclass(frozen=True)
class Skill:
    name: str
    description: str
    body: str
    directory: Path
    context: str | None = None
    policies: tuple[str, ...] = ()
    model: str | None = None


def collect_skill_roots() -> list[Path]:
    """Skill dirs in precedence order (highest first)."""
    roots: list[Path] = []
    seen: set[Path] = set()

    scope_all = os.getenv("SKILLS_SCOPE", "").lower() == "all"
    path = Path.cwd().resolve()
    for _ in range(12):
        for rel in RELATIVE_SKILL_ROOTS:
            candidate = path / rel
            _push_root(roots, seen, candidate)
        # Default scope is project-only: stop after the git worktree root.
        if not scope_all and (path / ".git").exists():
            break
        if path.parent == path:
            break
        path = path.parent

    for key in EXTRA_PATH_ENV_KEYS:
        raw = os.getenv(key, "")
        for part in raw.split(":"):
            trimmed = part.strip()
            if not trimmed:
                continue
            candidate = Path(trimmed)
            try:
                resolved = candidate.resolve()
            except OSError:
                resolved = candidate
            _push_root(roots, seen, resolved)

    return roots


def _push_root(roots: list[Path], seen: set[Path], candidate: Path) -> None:
    if not candidate.is_dir():
        return
    try:
        key = candidate.resolve()
    except OSError:
        key = candidate
    if key in seen:
        return
    seen.add(key)
    roots.append(key)


def find_skills_dir() -> Path:
    for root in collect_skill_roots():
        if root.name == "skills" and root.parent.name == ".claude":
            return root
    if collect_skill_roots():
        return collect_skill_roots()[0]
    return Path.cwd() / ".claude" / "skills"


def parse_frontmatter(text: str) -> tuple[dict[str, str], str]:
    """Parse YAML-ish frontmatter; multiline / list values flatten for policies."""
    if not text.startswith("---"):
        return {}, text
    parts = text.split("---", 2)
    if len(parts) < 3:
        return {}, text
    frontmatter: dict[str, str] = {}
    current_key: str | None = None
    for line in parts[1].splitlines():
        if line.startswith((" ", "\t")):
            trimmed = line.strip()
            if not trimmed or current_key is None:
                continue
            entry = frontmatter.get(current_key, "")
            if trimmed.startswith("-"):
                item = trimmed.lstrip("-").strip()
                frontmatter[current_key] = f"{entry}, {item}" if entry else item
            else:
                frontmatter[current_key] = f"{entry} {trimmed}".strip()
            continue
        if ":" not in line:
            current_key = None
            continue
        key, value = line.split(":", 1)
        current_key = key.strip()
        frontmatter[current_key] = value.strip()
    body = parts[2].lstrip("\n")
    return frontmatter, body


def _load_skills_from_dir(skills_dir: Path) -> list[Skill]:
    skills: list[Skill] = []
    try:
        entries = sorted(skills_dir.iterdir(), key=lambda p: p.name)
    except OSError:
        return []

    for entry in entries:
        if not entry.is_dir():
            continue
        skill_file = entry / "SKILL.md"
        if not skill_file.is_file():
            continue
        raw = skill_file.read_text(encoding="utf-8")
        frontmatter, body = parse_frontmatter(raw)
        name = frontmatter.get("name") or entry.name
        description = frontmatter.get("description", "")
        context = frontmatter.get("context")
        policies_raw = frontmatter.get("policies", "")
        policies = tuple(
            p.strip() for p in policies_raw.split(",") if p.strip()
        )
        model = frontmatter.get("model") or None
        try:
            directory = entry.resolve()
        except OSError:
            directory = entry
        skills.append(
            Skill(
                name=name,
                description=description,
                body=body,
                directory=directory,
                context=context,
                policies=policies,
                model=model,
            )
        )
    return skills


def discover_skills() -> list[Skill]:
    from app.agents import discover_agent_skills

    roots = collect_skill_roots()
    debug = os.getenv("SKILLS_DEBUG", "0") not in ("", "0")
    if debug:
        print(f"skills: scanning {len(roots)} root(s)", file=sys.stderr)
        for root in roots:
            print(f"  - {root}", file=sys.stderr)

    by_name: dict[str, Skill] = {}
    for root in roots:
        for skill in _load_skills_from_dir(root):
            if skill.name in by_name:
                existing = by_name[skill.name]
                print(
                    f"skills: ignoring duplicate '{skill.name}' from {skill.directory} "
                    f"(keeping {existing.directory})",
                    file=sys.stderr,
                )
                continue
            by_name[skill.name] = skill

    for skill in discover_agent_skills():
        if skill.name in by_name:
            existing = by_name[skill.name]
            print(
                f"skills: ignoring NomadWorks agent '{skill.name}' "
                f"(keeping {existing.directory})",
                file=sys.stderr,
            )
            continue
        if debug:
            print(
                f"skills: + agent fork `{skill.name}` (model={skill.model!r})",
                file=sys.stderr,
            )
        by_name[skill.name] = skill

    return [by_name[name] for name in sorted(by_name)]


def skill_by_name(skills: list[Skill], name: str) -> Skill | None:
    for skill in skills:
        if skill.name == name:
            return skill
    return None


def substitute_arguments(body: str, arguments: str) -> str:
    parts = arguments.split() if arguments else []

    def repl_index(match: re.Match[str]) -> str:
        idx = int(match.group(1))
        return parts[idx] if idx < len(parts) else ""

    # Indexed forms must run before bare $ARGUMENTS, or `$ARGUMENTS[0]` becomes `arg[0]`.
    result = re.sub(r"\$ARGUMENTS\[(\d+)\]", repl_index, body)
    result = result.replace("$ARGUMENTS", arguments)
    for idx, part in enumerate(parts):
        result = result.replace(f"${idx}", part)
    return result


def format_skill_catalog(skills: list[Skill]) -> str:
    if not skills:
        return ""
    lines = ["You have access to the following skills:", ""]
    for skill in skills:
        lines.append(f"- {skill.name}: {skill.description}")
    lines.extend(
        [
            "",
            "If a skill matches the user's request, call the Skill tool with its name",
            "and follow the instructions it returns.",
        ]
    )
    return "\n".join(lines)


@dataclass(frozen=True)
class SlashInvocation:
    skill_name: str
    body: str
    fork: bool = False


def fork_user_body(skill: Skill, arguments: str) -> str:
    """User message for a forked skill; append request when body has no placeholders."""
    body = substitute_arguments(skill.body, arguments)
    if not arguments:
        return body
    has_placeholder = (
        "$ARGUMENTS" in skill.body
        or "$0" in skill.body
        or bool(re.search(r"\$ARGUMENTS\[\d+\]", skill.body))
    )
    if has_placeholder:
        return body
    return f"{body}\n\n---\n\nUser request:\n{arguments}"


def expand_slash_invocation(
    prompt: str, skills: list[Skill]
) -> list[SlashInvocation] | None:
    """Return slash expansions, or None if not a slash prompt.

    A forked skill ends stacking and runs alone with remaining args.
    """
    stripped = prompt.lstrip()
    if not stripped.startswith("/"):
        return None

    tokens = stripped.split()
    expanded: list[str] = []
    arg_tokens: list[str] = []
    expanding = True
    fork_only: str | None = None

    for token in tokens:
        if not expanding:
            arg_tokens.append(token)
            continue
        if not token.startswith("/"):
            expanding = False
            arg_tokens.append(token)
            continue
        skill_name = token[1:]
        skill = skill_by_name(skills, skill_name)
        if skill is None:
            expanding = False
            arg_tokens.append(token)
            continue
        if skill.context == "fork":
            fork_only = skill_name
            expanding = False
            continue
        expanded.append(skill_name)

    if fork_only is not None:
        skill = skill_by_name(skills, fork_only)
        if skill is None:
            return None
        arguments = " ".join(arg_tokens)
        body = fork_user_body(skill, arguments)
        return [SlashInvocation(skill_name=fork_only, body=body, fork=True)]

    if not expanded:
        return None

    arguments = " ".join(arg_tokens)
    out: list[SlashInvocation] = []
    for name in expanded:
        skill = skill_by_name(skills, name)
        if skill is None:
            continue
        body = substitute_arguments(skill.body, arguments)
        if "scripts/" in body or "`scripts/" in body:
            try:
                rel = skill.directory.relative_to(Path.cwd())
            except ValueError:
                rel = skill.directory
            prefix = (
                f"Skill: {skill.name} (located at {rel})\n"
                "Paths in the instructions below are relative to that folder.\n\n"
            )
            body = prefix + body
        out.append(SlashInvocation(skill_name=name, body=body, fork=False))
    return out


def wrap_skill_body_for_location(skill: Skill, body: str) -> str:
    if "scripts/" not in body and "`scripts/" not in body:
        return body
    try:
        rel = skill.directory.relative_to(Path.cwd())
    except ValueError:
        rel = skill.directory
    return (
        f"Skill: {skill.name} (located at {rel})\n"
        "Paths in the instructions below are relative to that folder.\n\n"
        f"{body}"
    )
