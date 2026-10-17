const SECRET_KEYS = /^(.*_)?(API_KEY|SECRET|PASSWORD|TOKEN|PRIVATE_KEY|AUTHORIZATION)$/i

/** Redact secret-looking keys from a flat or nested config object. */
export function redactConfig(input: Record<string, unknown>): Record<string, unknown> {
  const out: Record<string, unknown> = {}
  for (const [k, v] of Object.entries(input)) {
    if (SECRET_KEYS.test(k)) {
      out[k] = typeof v === "string" && v.length ? "[redacted]" : v
    } else if (v && typeof v === "object" && !Array.isArray(v)) {
      out[k] = redactConfig(v as Record<string, unknown>)
    } else {
      out[k] = v
    }
  }
  return out
}
