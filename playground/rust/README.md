# Rust reference — full Claude Code clone

Preferred local playground on Apple Silicon (Mac Studio Ultra). Implements all **base** stages (Read, Write, Bash, agent loop) and the **Skills** extension through `mj2`.

Release builds use LTO + `target-cpu=native` via `your_program.sh`.

```bash
# Uses playground/.env → ollama.tokenizin.com (fallback 127.0.0.1:11434)
cd playground/rust
./your_program.sh -p "/smoke-echo"
```

See [docs/LOCAL_DEVELOPMENT.md](../../docs/LOCAL_DEVELOPMENT.md).
