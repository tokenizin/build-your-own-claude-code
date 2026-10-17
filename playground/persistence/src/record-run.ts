import { getDb } from "./client"
import { putBlob } from "./blob-store"
import { redactConfig } from "./redact"

export type NdjsonEvent = {
  type: string
  line?: string
  text?: string
  code?: number
  [key: string]: unknown
}

export type RecordRunInput = {
  kind: string
  cwd: string
  prompt: string
  agent?: string | null
  model?: string | null
  exitCode?: number | null
  finalText?: string | null
  events?: NdjsonEvent[]
  /** Raw NDJSON transcript to store as a blob */
  transcript?: string | null
  config?: Record<string, unknown> | null
  configName?: string
}

export async function upsertConfig(name: string, data: Record<string, unknown>, note?: string) {
  const db = getDb()
  const redacted = redactConfig(data)
  const existing = await db.playgroundConfig.findUnique({ where: { name } })
  if (existing) {
    return db.playgroundConfig.update({
      where: { id: existing.id },
      data: { data: redacted, note: note ?? existing.note },
    })
  }
  return db.playgroundConfig.create({
    data: { name, data: redacted, note },
  })
}

export async function recordRun(input: RecordRunInput) {
  const db = getDb()
  let configId: string | undefined
  if (input.config) {
    const cfg = await upsertConfig(input.configName || "default", input.config)
    configId = cfg.id
  }

  const success = (input.exitCode ?? 1) === 0
  const run = await db.toolRun.create({
    data: {
      kind: input.kind,
      cwd: input.cwd,
      prompt: input.prompt.slice(0, 16_000),
      agent: input.agent ?? undefined,
      model: input.model ?? undefined,
      exitCode: input.exitCode ?? undefined,
      success,
      finalText: input.finalText?.slice(0, 32_000) ?? undefined,
      finishedAt: new Date(),
      configId,
    },
  })

  const events = input.events ?? []
  for (let i = 0; i < events.length; i++) {
    const ev = events[i]!
    await db.toolEvent.create({
      data: {
        runId: run.id,
        type: String(ev.type || "unknown"),
        seq: i,
        payload: ev as Record<string, unknown>,
      },
    })
  }

  if (input.transcript && input.transcript.trim()) {
    const key = `runs/${run.id}/transcript.ndjson`
    const blob = await putBlob(input.transcript, {
      key,
      contentType: "application/x-ndjson",
      label: "transcript",
    })
    await db.blobObject.create({
      data: {
        runId: run.id,
        contentType: blob.contentType,
        sha256: blob.sha256,
        byteSize: blob.byteSize,
        uri: blob.uri,
        bucket: blob.bucket,
        objectKey: blob.objectKey,
        label: blob.label,
      },
    })
  }

  return run
}
