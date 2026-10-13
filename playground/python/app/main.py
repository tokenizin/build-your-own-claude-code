from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable

from openai import OpenAI

from app.agents import build_fork_messages, collect_nomadworks_roots
from app import nomad_tools
from app.skills import (
    Skill,
    discover_skills,
    expand_slash_invocation,
    fork_user_body,
    format_skill_catalog,
    skill_by_name,
    substitute_arguments,
    wrap_skill_body_for_location,
)


def _env_first(*keys: str, default: str) -> str:
    for key in keys:
        value = os.getenv(key)
        if value and value.strip():
            return value.strip()
    return default


def _normalize_v1_base(raw: str) -> str:
    trimmed = raw.strip().rstrip("/")
    return trimmed if trimmed.endswith("/v1") else f"{trimmed}/v1"


BASE_URL = _normalize_v1_base(
    _env_first(
        "OLLAMA_BASE_URL",
        "LLM_BASE_URL",
        "OPENROUTER_BASE_URL",
        default="https://ollama.tokenizin.com/v1",
    )
)
FALLBACK_BASE_URL = _normalize_v1_base(
    _env_first(
        "OLLAMA_FALLBACK_BASE_URL",
        "LLM_FALLBACK_BASE_URL",
        default="http://127.0.0.1:11434/v1",
    )
)
API_KEY = _env_first(
    "OLLAMA_API_KEY",
    "LLM_API_KEY",
    "OPENROUTER_API_KEY",
    default="ollama",
)
MODEL = _env_first(
    "OLLAMA_MODEL",
    "LLM_MODEL",
    "OPENROUTER_MODEL",
    default="gemma4:31b-cloud",
)
MAX_TOKENS = int(
    _env_first(
        "OLLAMA_MAX_TOKENS",
        "LLM_MAX_TOKENS",
        "OPENROUTER_MAX_TOKENS",
        default="2048",
    )
)

TOOL_SPECS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "Read",
            "description": "Read and return the contents of a file",
            "parameters": {
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The path to the file to read",
                    }
                },
                "required": ["file_path"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "Write",
            "description": "Write content to a file",
            "parameters": {
                "type": "object",
                "required": ["file_path", "content"],
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "The path of the file to write to",
                    },
                    "content": {
                        "type": "string",
                        "description": "The content to write to the file",
                    },
                },
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "Bash",
            "description": "Execute a shell command",
            "parameters": {
                "type": "object",
                "required": ["command"],
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command to execute",
                    }
                },
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "Skill",
            "description": "Load a skill's instructions into the conversation",
            "parameters": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The name of the skill to use",
                    },
                    "args": {
                        "type": "string",
                        "description": "Optional arguments for the skill",
                    },
                },
            },
        },
    },
    *nomad_tools.tool_specs(),
]

BASE_TOOL_SPECS = [
    t for t in TOOL_SPECS if t["function"]["name"] != "Skill"
]


def execute_read(args: dict[str, Any]) -> str:
    path = Path(args["file_path"])
    return path.read_text(encoding="utf-8")


def execute_write(args: dict[str, Any]) -> str:
    path = Path(args["file_path"])
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(args["content"], encoding="utf-8")
    return f"Successfully wrote to {path}"


def execute_bash(args: dict[str, Any]) -> str:
    command = args["command"]
    completed = subprocess.run(
        command,
        shell=True,
        capture_output=True,
        text=True,
        cwd=Path.cwd(),
    )
    output = completed.stdout
    if completed.stderr:
        output = f"{output}\n{completed.stderr}".strip()
    if completed.returncode != 0 and not output:
        output = f"Command failed with exit code {completed.returncode}"
    return output


def assistant_text(message: Any) -> str:
    content = getattr(message, "content", None)
    if isinstance(content, str) and content.strip():
        return content.strip()
    # OpenAI SDK may not surface Ollama's `reasoning`; check model_extra / dict.
    reasoning = None
    if hasattr(message, "model_extra") and isinstance(message.model_extra, dict):
        reasoning = message.model_extra.get("reasoning")
    if not reasoning and hasattr(message, "reasoning"):
        reasoning = getattr(message, "reasoning")
    if isinstance(reasoning, str) and reasoning.strip():
        lines = [ln.strip() for ln in reasoning.splitlines() if ln.strip()]
        if lines:
            return lines[-1].strip('`"')
    return ""


def probe_base(base_url: str, api_key: str) -> bool:
    url = f"{base_url.rstrip('/')}/models"
    req = urllib.request.Request(
        url,
        headers={"Authorization": f"Bearer {api_key}"},
        method="GET",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return 200 <= resp.status < 300
    except (urllib.error.URLError, TimeoutError, ValueError):
        return False


def resolve_client() -> tuple[OpenAI, str]:
    if probe_base(BASE_URL, API_KEY):
        print(f"LLM: {BASE_URL}", file=sys.stderr)
        return OpenAI(api_key=API_KEY, base_url=BASE_URL), BASE_URL
    if FALLBACK_BASE_URL != BASE_URL and probe_base(FALLBACK_BASE_URL, API_KEY):
        print(
            f"LLM: {BASE_URL} unreachable — using fallback {FALLBACK_BASE_URL}",
            file=sys.stderr,
        )
        return OpenAI(api_key=API_KEY, base_url=FALLBACK_BASE_URL), FALLBACK_BASE_URL
    print(
        f"LLM: could not reach {BASE_URL} (or fallback {FALLBACK_BASE_URL}); trying primary",
        file=sys.stderr,
    )
    return OpenAI(api_key=API_KEY, base_url=BASE_URL), BASE_URL


def run_agent_loop(
    client: OpenAI,
    messages: list[dict[str, Any]],
    skills: list[Skill],
    skill_handler: Callable[[str, str], str],
    *,
    tools: list[dict[str, Any]] | None = None,
    model: str | None = None,
    nomadflow_handler: Callable[[dict[str, Any]], str] | None = None,
) -> str:
    active_model = model or MODEL
    for _ in range(40):
        kwargs: dict[str, Any] = {
            "model": active_model,
            "messages": messages,
            "max_tokens": MAX_TOKENS,
        }
        if tools is not None:
            kwargs["tools"] = tools
        chat = client.chat.completions.create(**kwargs)
        if not chat.choices:
            raise RuntimeError("no choices in response")

        choice = chat.choices[0]
        message = choice.message
        text = assistant_text(message)
        assistant_payload: dict[str, Any] = {
            "role": "assistant",
            "content": text or message.content,
        }
        if message.tool_calls:
            assistant_payload["tool_calls"] = [
                {
                    "id": call.id,
                    "type": call.type,
                    "function": {
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    },
                }
                for call in message.tool_calls
            ]
        messages.append(assistant_payload)

        if not message.tool_calls:
            if not text:
                raise RuntimeError("assistant returned empty content")
            return text

        for call in message.tool_calls:
            fn_name = call.function.name
            fn_args = json.loads(call.function.arguments or "{}")
            if fn_name == "Read":
                result = execute_read(fn_args)
            elif fn_name == "Write":
                result = execute_write(fn_args)
            elif fn_name == "Bash":
                result = execute_bash(fn_args)
            elif fn_name == "Skill":
                result = skill_handler(fn_args.get("name", ""), fn_args.get("args", "") or "")
            elif fn_name == "nomadflow_run_workflow":
                if nomadflow_handler is None:
                    result = "Error: nomadflow_run_workflow handler not configured"
                else:
                    result = nomadflow_handler(fn_args)
            else:
                dispatched = nomad_tools.dispatch(fn_name, fn_args)
                result = dispatched if dispatched is not None else f"Unknown tool: {fn_name}"

            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": result,
                }
            )

    raise RuntimeError("agent loop exceeded maximum iterations")


def build_initial_run(
    prompt: str, skills: list[Skill]
) -> tuple[list[dict[str, Any]], list[dict[str, Any]] | None, str | None]:
    """Return (messages, tools, model_override)."""
    expanded = expand_slash_invocation(prompt, skills)
    if expanded is not None:
        if len(expanded) == 1 and expanded[0].fork:
            inv = expanded[0]
            skill = skill_by_name(skills, inv.skill_name)
            if skill is not None:
                messages = build_fork_messages(skill, inv.body)
                return messages, BASE_TOOL_SPECS, skill.model

        messages = [{"role": "user", "content": inv.body} for inv in expanded]
        needs_tools = any("scripts/" in inv.body for inv in expanded)
        if needs_tools:
            return messages, BASE_TOOL_SPECS, None
        return messages, None, None

    messages: list[dict[str, Any]] = []
    catalog = format_skill_catalog(skills)
    if catalog:
        messages.append({"role": "system", "content": catalog})
    messages.append({"role": "user", "content": prompt})
    return messages, TOOL_SPECS, None


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("-p", required=True)
    args = parser.parse_args()

    client, _active = resolve_client()

    skills = discover_skills()
    print(
        f"skills: {len(skills)} loaded (+ {len(collect_nomadworks_roots())} NomadWorks)",
        file=sys.stderr,
    )
    messages, tools, model_override = build_initial_run(args.p, skills)
    active_model = model_override or MODEL
    if model_override:
        print(f"model: {active_model} (fork override)", file=sys.stderr)
    else:
        print(f"model: {active_model}", file=sys.stderr)

    def skill_handler(name: str, skill_args: str) -> str:
        skill = skill_by_name(skills, name)
        if skill is None:
            return f"Unknown skill: {name}"

        if skill.context == "fork":
            body = fork_user_body(skill, skill_args)
            body = wrap_skill_body_for_location(skill, body)
            fork_messages = build_fork_messages(skill, body)
            if skill.model and skill.model != MODEL:
                print(f"fork `{skill.name}`: model {skill.model}", file=sys.stderr)
            answer = run_agent_loop(
                client,
                fork_messages,
                skills,
                skill_handler,
                tools=BASE_TOOL_SPECS,
                model=skill.model,
                nomadflow_handler=nomadflow_handler,
            )
            return f"Skill {skill.name} ran in a separate context and returned: {answer}"

        body = substitute_arguments(skill.body, skill_args)
        return wrap_skill_body_for_location(skill, body)

    def nomadflow_handler(fn_args: dict[str, Any]) -> str:
        task_path = (fn_args.get("task_path") or "").strip()
        instructions = (fn_args.get("instructions") or "").strip()
        if not task_path or not instructions:
            return "Error: nomadflow_run_workflow requires task_path and instructions."
        prompt = nomad_tools.workflow_fork_prompt(task_path, instructions)
        runner = skill_by_name(skills, "workflow_runner") or skill_by_name(skills, "runner")
        if runner is not None:
            fork_messages = build_fork_messages(runner, prompt)
            model = runner.model
        else:
            fork_messages = [
                {
                    "role": "system",
                    "content": "You are a NomadWorks workflow_runner in a forked playground context.",
                },
                {"role": "user", "content": prompt},
            ]
            model = None
        if model and model != MODEL:
            print(f"nomadflow: model {model}", file=sys.stderr)
        answer = run_agent_loop(
            client,
            fork_messages,
            skills,
            skill_handler,
            tools=BASE_TOOL_SPECS,
            model=model,
            nomadflow_handler=nomadflow_handler,
        )
        session = f"sync-{os.getpid()}-{id(fn_args)}"
        return (
            "SUCCESS: Workflow completed synchronously (playground shim).\n"
            f"Session: {session}\nTask: {task_path}\n---\n{answer}"
        )

    final = run_agent_loop(
        client,
        messages,
        skills,
        skill_handler,
        tools=tools,
        model=active_model,
        nomadflow_handler=nomadflow_handler,
    )
    print(final)


if __name__ == "__main__":
    main()
