"""NomadWorks agent roles → context:fork skills (P2)."""

from __future__ import annotations

import os
from pathlib import Path

from app.skills import Skill, parse_frontmatter

CORE_ROLES: list[tuple[str, tuple[str, ...]]] = [
    ("product_manager", ("pma",)),
    ("business_analyst", ("ba",)),
    ("tech_lead", ("tech-lead", "tl")),
    ("developer", ("dev",)),
    ("qa_engineer", ("qa",)),
    ("workflow_runner", ("runner", "wr")),
]

_DEFAULT_POLICIES: dict[str, tuple[str, ...]] = {
    "product_manager": (
        "starworld",
        "ecosystem.workflow",
        "ecosystem.a2a",
        "ecosystem.methodology",
        "hindsight-memory",
    ),
    "business_analyst": ("starworld", "hindsight-memory", "ecosystem.methodology"),
    "tech_lead": ("hindsight-memory", "starworld", "ecosystem.methodology"),
    "developer": (
        "starworld",
        "frontend-state-management",
        "hindsight-memory",
        "ecosystem.dapp-ui",
        "ecosystem.methodology",
    ),
    "qa_engineer": ("starworld", "ecosystem.methodology", "hindsight-memory"),
    "workflow_runner": ("starworld", "ecosystem.workflow", "ecosystem.methodology"),
}

_DEFAULT_MODEL = "qwen3.6:latest"


def collect_nomadworks_roots() -> list[Path]:
    roots: list[Path] = []
    seen: set[Path] = set()
    scope_all = os.getenv("SKILLS_SCOPE", "").lower() == "all"
    path = Path.cwd().resolve()
    for _ in range(12):
        candidate = path / ".nomadworks"
        if candidate.is_dir():
            try:
                key = candidate.resolve()
            except OSError:
                key = candidate
            if key not in seen:
                seen.add(key)
                roots.append(key)
        if not scope_all and (path / ".git").exists():
            break
        if path.parent == path:
            break
        path = path.parent

    raw = os.getenv("NOMADWORKS_PATH", "")
    for part in raw.split(":"):
        trimmed = part.strip()
        if not trimmed:
            continue
        candidate = Path(trimmed)
        try:
            resolved = candidate.resolve()
        except OSError:
            resolved = candidate
        if resolved.is_dir() and resolved not in seen:
            seen.add(resolved)
            roots.append(resolved)

    if not roots:
        starworld = os.getenv("STARWORLD", "").strip()
        if starworld:
            candidate = Path(starworld) / ".nomadworks"
            if candidate.is_dir():
                try:
                    roots.append(candidate.resolve())
                except OSError:
                    roots.append(candidate)

    return roots


def _policy_filename(policy_id: str) -> str:
    return f"{policy_id.replace('.', '-')}.md"


def load_policies(policy_ids: list[str], nomad_roots: list[Path]) -> str:
    max_total = int(os.getenv("NOMAD_POLICY_MAX_CHARS", "14000"))
    parts = ["# NomadWorks policies (injected for this forked agent)\n\n"]
    total = len(parts[0])

    for policy_id in policy_ids:
        filename = _policy_filename(policy_id)
        loaded = False
        for root in nomad_roots:
            path = root / "policies" / filename
            if not path.is_file():
                continue
            text = path.read_text(encoding="utf-8")
            header = f"## Policy: {policy_id}\n\n"
            if total + len(header) + len(text) > max_total:
                chunk = (
                    f"## Policy: {policy_id}\n\n"
                    f"[truncated — set NOMAD_POLICY_MAX_CHARS higher or Read {path}]\n\n"
                )
            else:
                chunk = f"{header}{text.strip()}\n\n"
            parts.append(chunk)
            total += len(chunk)
            loaded = True
            break
        if not loaded:
            chunk = f"## Policy: {policy_id}\n\n[missing — expected policies/{filename}]\n\n"
            parts.append(chunk)
            total += len(chunk)
    return "".join(parts)


def _parse_policy_list(raw: str) -> list[str]:
    items: list[str] = []
    for part in raw.replace("\n", ",").split(","):
        item = part.strip().lstrip("-").strip()
        if item:
            items.append(item)
    return items


def discover_agent_skills() -> list[Skill]:
    roots = collect_nomadworks_roots()
    if not roots:
        return []

    by_name: dict[str, Skill] = {}
    core_names = {name for name, _ in CORE_ROLES}

    for root in roots:
        agents_dir = root / "agents"
        if not agents_dir.is_dir():
            continue
        for path in sorted(agents_dir.glob("*.md")):
            if path.stem == "README":
                continue
            name = path.stem
            if name not in core_names:
                continue
            raw = path.read_text(encoding="utf-8")
            fm, body = parse_frontmatter(raw)
            description = fm.get("description") or f"NomadWorks agent role: {name}"
            policies = _parse_policy_list(fm.get("policies", ""))
            if not policies:
                policies = list(_DEFAULT_POLICIES.get(name, ("starworld", "ecosystem.methodology")))
            model = fm.get("model") or _DEFAULT_MODEL
            directory = path.parent
            skill = Skill(
                name=name,
                description=f"[NomadWorks fork] {description}",
                body=body,
                directory=directory,
                context="fork",
                policies=tuple(policies),
                model=model,
            )
            by_name.setdefault(name, skill)

    for canonical, aliases in CORE_ROLES:
        base = by_name.get(canonical)
        if base is None:
            continue
        for alias in aliases:
            if alias in by_name:
                continue
            by_name[alias] = Skill(
                name=alias,
                description=base.description,
                body=base.body,
                directory=base.directory,
                context=base.context,
                policies=base.policies,
                model=base.model,
            )

    return [by_name[n] for n in sorted(by_name)]


def build_fork_messages(skill: Skill, body: str) -> list[dict[str, str]]:
    roots = collect_nomadworks_roots()
    messages: list[dict[str, str]] = []
    if skill.policies:
        messages.append(
            {
                "role": "system",
                "content": load_policies(list(skill.policies), roots),
            }
        )
    messages.append(
        {
            "role": "system",
            "content": (
                f"You are running as NomadWorks agent/skill `{skill.name}` in a forked "
                "subagent context. You do not see the parent conversation. "
                "Use Read/Write/Bash when needed. When finished, respond with a concise "
                "final answer for the parent agent."
            ),
        }
    )
    messages.append({"role": "user", "content": body})
    return messages
