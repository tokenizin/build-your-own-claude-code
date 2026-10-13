"""NomadWorks tool shims for the playground (P3)."""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path
from typing import Any

from app.agents import collect_nomadworks_roots
from app.skills import parse_frontmatter

_SESSION_ID: str | None = None


def session_id() -> str:
    global _SESSION_ID
    if _SESSION_ID is None:
        _SESSION_ID = os.getenv("NOMAD_SESSION_ID") or f"playground-{os.getpid()}"
    return _SESSION_ID


def resolve_worktree() -> Path:
    cwd = Path.cwd()
    if (cwd / ".nomadworks").is_dir() or (cwd / "tasks").is_dir():
        return cwd
    starworld = os.getenv("STARWORLD", "").strip()
    if starworld and Path(starworld).is_dir():
        return Path(starworld)
    roots = collect_nomadworks_roots()
    if roots:
        return roots[0].parent
    return cwd


def tool_specs() -> list[dict[str, Any]]:
    return [
        {
            "type": "function",
            "function": {
                "name": "nomadworks_validate",
                "description": (
                    "Validate NomadWorks workflow artifacts and CodeMap integrity. "
                    "mode=phase0 (default) or mode=strict."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["phase0", "strict"],
                        }
                    },
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "nomadworks_start_discussion",
                "description": (
                    "Start or reopen a NomadWorks discussion under tasks/discussions/ "
                    "(playground file shim)."
                ),
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string"},
                        "existing_discussion_id": {"type": "string"},
                        "seed": {"type": "string"},
                    },
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "nomadworks_stop_discussion",
                "description": "Stop the active NomadWorks discussion for this playground session",
                "parameters": {"type": "object", "properties": {}},
            },
        },
        {
            "type": "function",
            "function": {
                "name": "nomadflow_run_workflow",
                "description": (
                    "Run a task lifecycle synchronously via a forked workflow_runner "
                    "(playground shim)."
                ),
                "parameters": {
                    "type": "object",
                    "required": ["task_path", "instructions"],
                    "properties": {
                        "task_path": {"type": "string"},
                        "instructions": {"type": "string"},
                    },
                },
            },
        },
        {
            "type": "function",
            "function": {
                "name": "nomadflow_prompt_workflow",
                "description": (
                    "Follow-up to a workflow. Playground is synchronous — call "
                    "nomadflow_run_workflow again."
                ),
                "parameters": {
                    "type": "object",
                    "required": ["session_id", "text"],
                    "properties": {
                        "session_id": {"type": "string"},
                        "text": {"type": "string"},
                    },
                },
            },
        },
    ]


def dispatch(name: str, args: dict[str, Any]) -> str | None:
    if name == "nomadworks_validate":
        return execute_validate(args)
    if name == "nomadworks_start_discussion":
        return execute_start_discussion(args)
    if name == "nomadworks_stop_discussion":
        return execute_stop_discussion()
    if name == "nomadflow_prompt_workflow":
        return execute_prompt_workflow(args)
    if name == "nomadflow_run_workflow":
        return None  # handled async/sync in main
    return None


def _truncate(text: str, max_chars: int) -> str:
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + f"\n… [truncated at {max_chars} chars]"


def execute_validate(args: dict[str, Any]) -> str:
    mode = (args.get("mode") or "phase0").strip()
    worktree = resolve_worktree()
    if mode == "strict":
        return _validate_strict(worktree)
    return _validate_phase0(worktree)


def _validate_phase0(worktree: Path) -> str:
    script = worktree / "scripts" / "validate-nomadworks.sh"
    if not script.is_file():
        return (
            f"FAIL: {script} not found. Run from StarWorld (or set STARWORLD) "
            "for Phase 0 validate."
        )
    proc = subprocess.run(
        ["bash", str(script)],
        cwd=worktree,
        env={**os.environ, "NOMADWORKS_STRICT_CODEMAP": "0"},
        capture_output=True,
        text=True,
    )
    text = (proc.stdout or "") + (("\n" + proc.stderr) if proc.stderr else "")
    return _truncate(text, 8000)


def _validate_strict(worktree: Path) -> str:
    pkg = worktree / "node_modules/@neuralnomads/nomadworks/dist/validate_logic.js"
    if not pkg.is_file():
        return (
            f"FAIL: {pkg} missing. Install @neuralnomads/nomadworks in the worktree first."
        )
    pkg_url = pkg.resolve().as_uri()
    workdir = json.dumps(str(worktree))
    js = f"""
import {{ nomadworks_validate_logic }} from '{pkg_url}';
const r = await nomadworks_validate_logic({workdir});
const max = 40;
if (r.ok) {{
  console.log('PASS: All source directories indexed. Hierarchy validated.');
  console.log('Warnings: ' + r.warnings.length);
  for (const w of r.warnings.slice(0, max)) console.log('- ' + w);
}} else {{
  console.log('FAIL: Validation errors found: ' + r.errors.length);
  for (const e of r.errors.slice(0, max)) console.log('- ' + e);
  if (r.errors.length > max) console.log('... and ' + (r.errors.length - max) + ' more');
}}
"""
    proc = subprocess.run(
        ["node", "--input-type=module", "-e", js],
        cwd=worktree,
        capture_output=True,
        text=True,
    )
    text = (proc.stdout or "") + (("\n" + proc.stderr) if proc.stderr else "")
    if not text.strip():
        text = f"FAIL: strict validate produced no output (exit {proc.returncode})"
    return _truncate(text, 12000)


def _slugify(title: str) -> str:
    s = re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")[:80]
    return s or "discussion"


def _registry_path(worktree: Path) -> Path:
    return worktree / ".codenomad" / "runtime" / "discussions.json"


def _load_registry(worktree: Path) -> dict[str, Any]:
    path = _registry_path(worktree)
    if not path.is_file():
        return {"version": 1, "active": {}}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return {"version": 1, "active": data.get("active") or {}}
    except (OSError, json.JSONDecodeError):
        return {"version": 1, "active": {}}


def _save_registry(worktree: Path, registry: dict[str, Any]) -> None:
    path = _registry_path(worktree)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(registry, indent=2) + "\n", encoding="utf-8")


def _write_discussion_md(path: Path, fm: dict[str, str], body: str) -> None:
    keys = ["id", "title", "status", "agent", "session_id"]
    lines = ["---"]
    for key in keys:
        if key in fm:
            lines.append(f"{key}: {fm[key]}")
    for key, value in fm.items():
        if key in keys or key == "appended_message_ids":
            continue
        lines.append(f"{key}: {value}")
    lines.append("---")
    lines.append("")
    lines.append(body.rstrip())
    lines.append("")
    path.write_text("\n".join(lines), encoding="utf-8")


def execute_start_discussion(args: dict[str, Any]) -> str:
    worktree = resolve_worktree()
    sid = session_id()
    title = (args.get("title") or "").strip()
    existing = (args.get("existing_discussion_id") or "").strip()
    seed = (args.get("seed") or "").strip()

    if (not title and not existing) or (title and existing):
        return "Error: Provide exactly one of 'title' or 'existing_discussion_id'."

    registry = _load_registry(worktree)
    if sid in registry["active"]:
        ex = registry["active"][sid]
        return (
            "FAIL: An active discussion already exists for this session.\n"
            f"ID: {ex.get('id')}\nTitle: {ex.get('title')}\n"
            f"File: {ex.get('filePath')}\nStatus: {ex.get('status')}"
        )

    discussions_dir = worktree / "tasks" / "discussions"
    discussions_dir.mkdir(parents=True, exist_ok=True)

    if existing:
        matches = sorted(
            n.name
            for n in discussions_dir.iterdir()
            if n.name.startswith(f"{existing}-") and n.name.endswith(".md")
        )
        if not matches:
            return f"Error: Discussion '{existing}' was not found."
        filename = matches[0]
        relative = f"tasks/discussions/{filename}"
        absolute = worktree / relative
        raw = absolute.read_text(encoding="utf-8")
        fm, body = parse_frontmatter(raw)
        fm["status"] = "active"
        fm["agent"] = "playground"
        fm["session_id"] = sid
        _write_discussion_md(absolute, fm, body)
        discussion_title = fm.get("title") or existing
        action = "reopened"
        disc_id = existing
    else:
        sequence = 1
        while True:
            disc_id = f"DISCUSSION-{sequence:03d}"
            filename = f"{disc_id}-{_slugify(title)}.md"
            relative = f"tasks/discussions/{filename}"
            absolute = worktree / relative
            if not absolute.exists():
                break
            sequence += 1
            if sequence > 9999:
                return "FAIL: could not allocate discussion id"
        fm = {
            "id": disc_id,
            "title": title,
            "status": "active",
            "agent": "playground",
            "session_id": sid,
        }
        body = f"# Discussion: {title}\n\n## Messages\n"
        _write_discussion_md(absolute, fm, body)
        discussion_title = title
        action = "started"

    if seed:
        raw = absolute.read_text(encoding="utf-8")
        fm, body = parse_frontmatter(raw)
        entry = f"**User**\n{seed}"
        next_body = f"{body.rstrip()}\n\n{entry}" if body.strip() else entry
        _write_discussion_md(absolute, fm, next_body)

    registry["active"][sid] = {
        "id": disc_id,
        "title": discussion_title,
        "filePath": relative,
        "status": "active",
        "agent": "playground",
        "appendedMessageIDs": [],
    }
    _save_registry(worktree, registry)
    return (
        f"SUCCESS: Discussion {action}.\nID: {disc_id}\nTitle: {discussion_title}\n"
        f"File: {relative}\nStatus: active\nSession: {sid}"
    )


def execute_stop_discussion() -> str:
    worktree = resolve_worktree()
    sid = session_id()
    registry = _load_registry(worktree)
    existing = registry["active"].get(sid)
    if not existing:
        return "FAIL: No active discussion exists for this session."
    file_path = existing.get("filePath") or ""
    absolute = worktree / file_path
    if absolute.is_file():
        raw = absolute.read_text(encoding="utf-8")
        fm, body = parse_frontmatter(raw)
        fm["status"] = "closing"
        _write_discussion_md(absolute, fm, body)
    existing["status"] = "closing"
    _save_registry(worktree, registry)
    return (
        "SUCCESS: Discussion stop requested.\n"
        f"ID: {existing.get('id')}\nTitle: {existing.get('title')}\n"
        f"File: {file_path}\nStatus: closing"
    )


def execute_prompt_workflow(args: dict[str, Any]) -> str:
    session = args.get("session_id") or ""
    text = args.get("text") or ""
    return (
        "FAIL: Playground workflows are synchronous (no OpenCode background session).\n"
        f"Session '{session}' is not a live runner.\n"
        "Call nomadflow_run_workflow again with updated instructions instead.\n"
        f"Follow-up text was:\n{text}"
    )


def workflow_fork_prompt(task_path: str, instructions: str) -> str:
    worktree = resolve_worktree()
    abs_path = Path(task_path) if Path(task_path).is_absolute() else worktree / task_path
    try:
        task_body = abs_path.read_text(encoding="utf-8")
    except OSError as exc:
        task_body = f"[could not read task file {abs_path}: {exc}]"
    return (
        "You are the NomadWorks workflow_runner in a forked playground session.\n"
        f"Task File: {task_path}\n\n"
        f"## Task file contents\n\n{task_body}\n\n"
        f"## Instructions\n\n{instructions}\n\n"
        "Execute the appropriate lifecycle for this task. Use Read/Write/Bash and "
        "nomadworks_validate when needed. When finished, respond with a concise final summary."
    )
