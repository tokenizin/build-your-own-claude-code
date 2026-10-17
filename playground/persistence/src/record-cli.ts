/**
 * Read NDJSON transcript from stdin (or --file) and persist a ToolRun.
 *
 * Usage:
 *   bridge --jsonl … | bun run src/record-cli.ts --kind bridge
 *   bun run src/record-cli.ts --kind bridge --file transcript.ndjson
 */
import { readFileSync } from "node:fs"
import { closeDb } from "./client"
import { recordRun, type NdjsonEvent } from "./record-run"

function parseArgs(argv: string[]) {
  let kind = "bridge"
  let file: string | undefined
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--kind") kind = argv[++i] || kind
    else if (argv[i] === "--file") file = argv[++i]
  }
  return { kind, file }
}

const { kind, file } = parseArgs(process.argv.slice(2))
const raw = file
  ? readFileSync(file, "utf8")
  : await new Response(Bun.stdin.stream()).text()

const events: NdjsonEvent[] = []
for (const line of raw.split(/\r?\n/)) {
  if (!line.trim()) continue
  try {
    events.push(JSON.parse(line) as NdjsonEvent)
  } catch {
    events.push({ type: "parse_error", line })
  }
}

const meta = events.find((e) => e.type === "meta") || {}
const final = events.find((e) => e.type === "final")
const exit = events.find((e) => e.type === "exit")
const modelLine = events.find(
  (e) => e.type === "stderr" && typeof e.line === "string" && e.line.startsWith("model:"),
)

const run = await recordRun({
  kind,
  cwd: String(meta.cwd || process.cwd()),
  prompt: String(meta.prompt || ""),
  agent: (meta.agent as string | null) ?? null,
  model:
    typeof modelLine?.line === "string"
      ? modelLine.line.replace(/^model:\s*/, "").trim()
      : null,
  exitCode: typeof exit?.code === "number" ? exit.code : 1,
  finalText: typeof final?.text === "string" ? final.text : null,
  events,
  transcript: raw,
  config: {
    OLLAMA_BASE_URL: process.env.OLLAMA_BASE_URL,
    OLLAMA_MODEL: process.env.OLLAMA_MODEL,
    OLLAMA_API_KEY: process.env.OLLAMA_API_KEY,
    STARWORLD: process.env.STARWORLD,
    CLAUDE_CODE_PLAYGROUND: process.env.CLAUDE_CODE_PLAYGROUND,
  },
})

console.log(JSON.stringify({ ok: true, runId: run.id, events: events.length }))
await closeDb()
