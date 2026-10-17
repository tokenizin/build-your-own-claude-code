import { ZenStackClient } from "@zenstackhq/orm"
import { PostgresDialect } from "@zenstackhq/orm/dialects/postgres"
import { Pool } from "pg"
import { schema } from "../generated/schema"
import { DATABASE_URL } from "./env"

let _db: ZenStackClient<typeof schema> | undefined
let _pool: Pool | undefined

export function getDb() {
  if (_db) return _db
  _pool = new Pool({ connectionString: DATABASE_URL() })
  _db = new ZenStackClient(schema, {
    dialect: new PostgresDialect({ pool: _pool }),
  })
  return _db
}

export async function closeDb() {
  await _pool?.end()
  _pool = undefined
  _db = undefined
}
