import { ensureBucket } from "./blob-store"

await ensureBucket()
console.log("MinIO bucket ready:", process.env.MINIO_BUCKET || "claude-code-playground")
