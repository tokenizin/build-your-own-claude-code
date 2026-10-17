#!/usr/bin/env python3
"""CodeNomad ↔ playground/rust bridge (P4).

Spawns playground/rust/your_program.sh and streams output.
With --jsonl, emits NDJSON: meta | stderr | stdout | final | exit.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import threading
from pathlib import Path


def repo_root() -> Path:
    env = os.environ.get("CLAUDE_CODE_PLAYGROUND", "").strip()
    if env:
        return Path(env).resolve()
    return Path(__file__).resolve().parent.parent


def emit(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, ensure_ascii=False) + "\n")
    sys.stdout.flush()


def stream_pipe(pipe, event_type: str, sink: list[str], jsonl: bool) -> None:
    assert pipe is not None
    for raw in iter(pipe.readline, ""):
        line = raw.rstrip("\n")
        sink.append(line)
        if jsonl:
            emit({"type": event_type, "line": line})
        else:
            stream = sys.stderr if event_type == "stderr" else sys.stdout
            stream.write(raw)
            stream.flush()
    pipe.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="CodeNomad rust playground bridge")
    parser.add_argument("-p", "--prompt", required=True)
    parser.add_argument("--cwd", default=os.getcwd())
    parser.add_argument("--agent", default="", help="Optional slash role: pma, ba, runner, …")
    parser.add_argument("--jsonl", action="store_true")
    parser.add_argument(
        "--persist",
        action="store_true",
        help="Record NDJSON transcript into local Neon + MinIO (playground/persistence)",
    )
    args = parser.parse_args()
    if os.environ.get("PLAYGROUND_PERSIST", "").strip() in ("1", "true", "yes"):
        args.persist = True

    root = repo_root()
    runner = root / "playground" / "rust" / "your_program.sh"
    if not runner.is_file():
        print(f"ERROR: runner not found: {runner}", file=sys.stderr)
        print("Set CLAUDE_CODE_PLAYGROUND to the course repo root.", file=sys.stderr)
        return 2

    prompt = args.prompt
    agent = (args.agent or "").lstrip("/")
    if agent and not prompt.lstrip().startswith("/"):
        prompt = f"/{agent} {prompt}"

    cwd = Path(args.cwd).resolve()
    if not cwd.is_dir():
        print(f"ERROR: cwd is not a directory: {cwd}", file=sys.stderr)
        return 2

    if args.jsonl:
        emit(
            {
                "type": "meta",
                "cwd": str(cwd),
                "prompt": prompt,
                "runner": str(runner),
                "agent": agent or None,
            }
        )

    # Ensure executable bit for the shell wrapper.
    try:
        runner.chmod(runner.stat().st_mode | 0o111)
    except OSError:
        pass

    proc = subprocess.Popen(
        [str(runner), "-p", prompt],
        cwd=str(cwd),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
        env=os.environ.copy(),
    )

    stdout_lines: list[str] = []
    stderr_lines: list[str] = []
    t_out = threading.Thread(
        target=stream_pipe, args=(proc.stdout, "stdout", stdout_lines, args.jsonl), daemon=True
    )
    t_err = threading.Thread(
        target=stream_pipe, args=(proc.stderr, "stderr", stderr_lines, args.jsonl), daemon=True
    )
    t_out.start()
    t_err.start()
    code = proc.wait()
    t_out.join(timeout=5)
    t_err.join(timeout=5)

    final = "\n".join(stdout_lines).rstrip("\n")
    transcript_lines: list[str] = []
    if args.jsonl or args.persist:
        # Rebuild a complete NDJSON transcript for persistence / consumers.
        meta = {
            "type": "meta",
            "cwd": str(cwd),
            "prompt": prompt,
            "runner": str(runner),
            "agent": agent or None,
        }
        transcript_lines.append(json.dumps(meta, ensure_ascii=False))
        for line in stderr_lines:
            transcript_lines.append(
                json.dumps({"type": "stderr", "line": line}, ensure_ascii=False)
            )
        for line in stdout_lines:
            transcript_lines.append(
                json.dumps({"type": "stdout", "line": line}, ensure_ascii=False)
            )
        transcript_lines.append(json.dumps({"type": "final", "text": final}, ensure_ascii=False))
        transcript_lines.append(
            json.dumps({"type": "exit", "code": code}, ensure_ascii=False)
        )

    if args.jsonl:
        emit({"type": "final", "text": final})
        emit({"type": "exit", "code": code})

    if args.persist:
        persist_dir = root / "playground" / "persistence"
        record_cli = persist_dir / "src" / "record-cli.ts"
        if record_cli.is_file():
            try:
                subprocess.run(
                    ["bun", "run", str(record_cli), "--kind", "bridge"],
                    cwd=str(persist_dir),
                    input="\n".join(transcript_lines) + "\n",
                    text=True,
                    check=False,
                    env=os.environ.copy(),
                )
            except FileNotFoundError:
                print("WARN: bun not found; skip persistence", file=sys.stderr)
        else:
            print(f"WARN: persistence recorder missing: {record_cli}", file=sys.stderr)

    return code if code is not None else 1


if __name__ == "__main__":
    sys.exit(main())
