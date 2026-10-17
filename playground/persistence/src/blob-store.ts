import { createHash } from "node:crypto"
import {
  CreateBucketCommand,
  HeadBucketCommand,
  PutObjectCommand,
  S3Client,
} from "@aws-sdk/client-s3"
import { MINIO } from "./env"

export function getS3(): S3Client {
  const endpoint = MINIO.endpoint()
  const proto = MINIO.useSsl() ? "https" : "http"
  return new S3Client({
    region: MINIO.region(),
    endpoint: `${proto}://${endpoint}`,
    forcePathStyle: true,
    credentials: {
      accessKeyId: MINIO.accessKey(),
      secretAccessKey: MINIO.secretKey(),
    },
  })
}

export async function ensureBucket(client = getS3(), bucket = MINIO.bucket()) {
  try {
    await client.send(new HeadBucketCommand({ Bucket: bucket }))
  } catch {
    await client.send(new CreateBucketCommand({ Bucket: bucket }))
  }
  return bucket
}

export async function putBlob(
  body: Buffer | string,
  opts: { key: string; contentType?: string; label?: string },
) {
  const client = getS3()
  const bucket = await ensureBucket(client)
  const buf = typeof body === "string" ? Buffer.from(body, "utf8") : body
  const sha256 = createHash("sha256").update(buf).digest("hex")
  const contentType = opts.contentType ?? "application/octet-stream"
  await client.send(
    new PutObjectCommand({
      Bucket: bucket,
      Key: opts.key,
      Body: buf,
      ContentType: contentType,
      Metadata: opts.label ? { label: opts.label } : undefined,
    }),
  )
  return {
    bucket,
    objectKey: opts.key,
    uri: `s3://${bucket}/${opts.key}`,
    sha256,
    byteSize: buf.byteLength,
    contentType,
    label: opts.label,
  }
}
