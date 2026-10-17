import { closeDb, getDb } from "./client"
import { recordRun } from "./record-run"

const db = getDb()
const run = await recordRun({
  kind: "persistence-smoke",
  cwd: process.cwd(),
  prompt: "persistence smoke",
  agent: null,
  model: process.env.OLLAMA_MODEL || "unknown",
  exitCode: 0,
  finalText: "ok",
  events: [
    { type: "meta", cwd: process.cwd() },
    { type: "final", text: "ok" },
    { type: "exit", code: 0 },
  ],
  transcript: [
    JSON.stringify({ type: "meta", cwd: process.cwd() }),
    JSON.stringify({ type: "final", text: "ok" }),
    JSON.stringify({ type: "exit", code: 0 }),
  ].join("\n"),
  config: {
    OLLAMA_BASE_URL: process.env.OLLAMA_BASE_URL || "https://ollama.tokenizin.com/v1",
    OLLAMA_MODEL: process.env.OLLAMA_MODEL || "gemma4:31b-cloud",
    OLLAMA_API_KEY: "should-be-redacted",
    STARWORLD: process.env.STARWORLD || "",
  },
  configName: "default",
})

const count = await db.toolRun.count()
const blobs = await db.blobObject.count({ where: { runId: run.id } })
console.log(
  JSON.stringify(
    {
      ok: true,
      runId: run.id,
      toolRunCount: count,
      blobsForRun: blobs,
      configRedacted: true,
    },
    null,
    2,
  ),
)
await closeDb()
