"""Load OpenCode / CodeNomad agent catalogs for the Python playground (parity with Rust)."""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class OpencodeAdapter:
    default_fork_model: str = "qwen3.6:latest"
    small_model: str | None = "qwen3:8b"
    model_remap: dict[str, str] = field(default_factory=dict)
    aliases: dict[str, str] = field(default_factory=dict)
    core_fallback: list[str] = field(default_factory=list)
    agent_mode: str = "opencode"


def _default_adapter() -> OpencodeAdapter:
    return OpencodeAdapter(
        model_remap={
            "opencode/space-bunny-free": "qwen3.6:latest",
            "openrouter-fallback/auto": "qwen3.6:latest",
            "opencode-go/mimo-v2.5": "qwen3.6:latest",
            "ollama/qwen3:8b": "qwen3:8b",
            "ollama/qwen3.6:latest": "qwen3.6:latest",
        },
        aliases={
            "pma": "product_manager",
            "ba": "business_analyst",
            "tl": "tech_lead",
            "tech-lead": "tech_lead",
            "dev": "developer",
            "qa": "qa_engineer",
            "runner": "workflow_runner",
            "wr": "workflow_runner",
        },
        core_fallback=[
            "product_manager",
            "business_analyst",
            "tech_lead",
            "developer",
            "qa_engineer",
            "workflow_runner",
        ],
    )


def expand_env_path(raw: str) -> str:
    out = raw
    sw = os.getenv("STARWORLD", "").strip()
    if sw:
        out = out.replace("${STARWORLD}", sw).replace("$STARWORLD", sw)
    root = os.getenv("CLAUDE_CODE_PLAYGROUND", "").strip()
    if root:
        out = out.replace("${CLAUDE_CODE_PLAYGROUND}", root).replace(
            "$CLAUDE_CODE_PLAYGROUND", root
        )
    return out


def load_adapter() -> OpencodeAdapter:
    candidates = []
    if os.getenv("OPENCODE_RUST_ADAPTER"):
        candidates.append(Path(os.environ["OPENCODE_RUST_ADAPTER"]))
    if os.getenv("CLAUDE_CODE_PLAYGROUND"):
        candidates.append(
            Path(os.environ["CLAUDE_CODE_PLAYGROUND"]) / "playground/opencode-rust.json"
        )
    candidates.extend(
        [
            Path("playground/opencode-rust.json"),
            Path("../opencode-rust.json"),
            Path("../../playground/opencode-rust.json"),
            Path(
                "/Users/alexshapiro/0-claudecode/build-your-own-claude-code/playground/opencode-rust.json"
            ),
        ]
    )
    base = _default_adapter()
    for path in candidates:
        if not path.is_file():
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        if "defaultForkModel" in data:
            base.default_fork_model = data["defaultForkModel"]
        if "smallModel" in data:
            base.small_model = data["smallModel"]
        if "agentMode" in data:
            base.agent_mode = data["agentMode"]
        if isinstance(data.get("modelRemap"), dict):
            base.model_remap.update({str(k): str(v) for k, v in data["modelRemap"].items()})
        if isinstance(data.get("aliases"), dict):
            base.aliases = {str(k): str(v) for k, v in data["aliases"].items()}
        if isinstance(data.get("coreFallback"), list):
            base.core_fallback = [str(x) for x in data["coreFallback"]]
        return base
    return base


def strip_jsonc(text: str) -> str:
    out: list[str] = []
    i = 0
    n = len(text)
    in_str = False
    escape = False
    while i < n:
        c = text[i]
        if in_str:
            out.append(c)
            if escape:
                escape = False
            elif c == "\\":
                escape = True
            elif c == '"':
                in_str = False
            i += 1
            continue
        if c == '"':
            in_str = True
            out.append(c)
            i += 1
            continue
        if c == "/" and i + 1 < n:
            nxt = text[i + 1]
            if nxt == "/":
                i += 2
                while i < n and text[i] != "\n":
                    i += 1
                continue
            if nxt == "*":
                i += 2
                while i + 1 < n and not (text[i] == "*" and text[i + 1] == "/"):
                    i += 1
                i = min(i + 2, n)
                continue
        out.append(c)
        i += 1
    cleaned = "".join(out)
    cleaned = re.sub(r",(\s*[}\]])", r"\1", cleaned)
    return cleaned


def resolve_opencode_config_path() -> Path | None:
    candidates: list[Path] = []
    if os.getenv("OPENCODE_CONFIG"):
        candidates.append(Path(expand_env_path(os.environ["OPENCODE_CONFIG"])))
    sw = os.getenv("STARWORLD", "").strip()
    if sw:
        candidates.extend(
            [
                Path(sw) / "CodeNomad/.opencode/opencode.jsonc",
                Path(sw) / "opencode.json",
                Path(sw) / ".opencode/opencode.json",
            ]
        )
    candidates.extend(
        [
            Path("/Users/alexshapiro/starworld/CodeNomad/.opencode/opencode.jsonc"),
            Path("/Users/alexshapiro/starworld/opencode.json"),
        ]
    )
    for path in candidates:
        if path.is_file():
            try:
                return path.resolve()
            except OSError:
                return path
    return None


def remap_model(adapter: OpencodeAdapter, raw: str | None) -> str | None:
    if not raw or not raw.strip():
        return adapter.default_fork_model
    raw = raw.strip()
    if raw in adapter.model_remap:
        return adapter.model_remap[raw]
    if raw.startswith("ollama/"):
        return raw[len("ollama/") :]
    if "/" in raw:
        return adapter.default_fork_model
    return raw


def agent_mode_setting(adapter: OpencodeAdapter) -> str:
    return (
        os.getenv("NOMAD_AGENTS")
        or os.getenv("PLAYGROUND_AGENTS")
        or adapter.agent_mode
    ).lower()


def parse_opencode_agents(config_path: Path) -> tuple[dict, list[dict]]:
    raw = config_path.read_text(encoding="utf-8")
    data = json.loads(strip_jsonc(raw))
    config_dir = config_path.parent
    specs: list[dict] = []
    for name, entry in (data.get("agent") or {}).items():
        if not isinstance(entry, dict):
            continue
        prompt = entry.get("prompt") or ""
        inner = prompt
        if prompt.startswith("{file:") and prompt.endswith("}"):
            inner = prompt[len("{file:") : -1]
        path = Path(inner)
        if not path.is_absolute():
            path = config_dir / path
        if not path.is_file():
            print(f"opencode: skip agent `{name}` — prompt not found: {prompt}", flush=True)
            continue
        try:
            prompt_path = path.resolve()
        except OSError:
            prompt_path = path
        specs.append(
            {
                "name": name,
                "description": entry.get("description") or name,
                "mode": entry.get("mode") or "subagent",
                "model": entry.get("model"),
                "prompt_path": prompt_path,
            }
        )
    specs.sort(key=lambda s: s["name"])
    return data, specs
