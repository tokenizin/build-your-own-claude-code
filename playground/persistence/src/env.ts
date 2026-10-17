import { readFileSync, existsSync } from "node:fs"
import { resolve, dirname } from "node:path"
import { fileURLToPath } from "node:url"

const here = dirname(fileURLToPath(import.meta.url))
const root = resolve(here, "..")

function loadEnvFile(path: string) {
  if (!existsSync(path)) return
  for (const line of readFileSync(path, "utf8").split(/\r?\n/)) {
    const trimmed = line.trim()
    if (!trimmed || trimmed.startsWith("#")) continue
    const eq = trimmed.indexOf("=")
    if (eq < 0) continue
    const key = trimmed.slice(0, eq).trim()
    let value = trimmed.slice(eq + 1).trim()
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1)
    }
    if (!(key in process.env)) process.env[key] = value
  }
}

loadEnvFile(resolve(root, ".env"))
loadEnvFile(resolve(root, "../.env"))

export function requireEnv(key: string, fallback?: string): string {
  const v = process.env[key]?.trim() || fallback
  if (!v) throw new Error(`Missing env ${key}`)
  return v
}

export const DATABASE_URL = () =>
  requireEnv(
    "DATABASE_URL",
    "postgresql://neondb_owner:npg_local_dev_only@127.0.0.1:54329/claude_code_playground",
  )

export const MINIO = {
  endpoint: () => requireEnv("MINIO_ENDPOINT", "127.0.0.1:9000"),
  bucket: () => requireEnv("MINIO_BUCKET", "claude-code-playground"),
  accessKey: () => requireEnv("MINIO_ACCESS_KEY", "minioadmin"),
  secretKey: () => requireEnv("MINIO_SECRET_KEY", "minioadmin123"),
  region: () => requireEnv("MINIO_REGION", "us-east-1"),
  useSsl: () => (process.env.MINIO_USE_SSL || "0") === "1",
}
