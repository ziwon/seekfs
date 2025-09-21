# seekfs
Object‑Storage‑Native Training Data Plane (Rust)

## Highlights
- Namespace + manifest v1 (HEAD → vN)
- Blob API: GET /blob, POST /readv
- L2 cache backends: in‑memory (dev) and disk segmentstore (aka slabfile)
- S3 clients: AWS SDK (default) or AWS CRT (feature)
- Per‑page CRC32C validation, file‑level SHA‑256 in manifest

## Prerequisites
- Rust 1.90+
- For CRT: cmake, gcc/g++, make
- Optional dev stack: kind, kubectl, helm, mc (MinIO client)

## Quickstart

1) Start MinIO in kind
- `just kind-up`
- `just minio-apply`
- `just minio-port-forward`  # exposes http://localhost:9000

2) Configure seekfs (seekfs.toml)
- Segmentstore is the default L2 at `/var/lib/seekfs/l2`. Ensure daemon can write there, or change the path.

3) Build & run
- `just build`
- `just run-crt-slab`  # CRT + segmentstore; or `just run-sdk`

4) Upload and generate a manifest
- `just mc-alias && just mc-mb`
- `just mc-upload-ns /tmp/warm.bin warm.bin dev`
- `just manifest-gen dev warm.bin`

5) Exercise the API
- HEAD: `curl 'http://127.0.0.1:7070/test_head?ns=dev&path=warm.bin'`
- Blob: `curl 'http://127.0.0.1:7070/blob?ns=dev&path=warm.bin&off=0&len=4096' | wc -c`
- Readv: `curl -s -X POST 'http://127.0.0.1:7070/readv?ns=dev' -H 'content-type: application/json' -d '[{"path":"warm.bin","off":0,"len":4096}]' | wc -c`

## Configuration (seekfs.toml)

```toml
[cache]
slab_size_mib = 8
l1_memory_gib = 32

[cache.l2]
backend = "segmentstore"                 # alias: "slabfile"
segment_path = "/var/lib/seekfs/l2"     # ensure write permissions

[s3]
engine = "sdk"                           # sdk | crt
endpoint = "http://localhost:9000"
region = "us-east-1"
bucket = "seekfs"
access_key_id = "minioadmin"
secret_access_key = "minioadmin"
get_budget_per_sec = 200
range_coalesce_max_mib = 32
max_inflight_gets = 128
retry_max_attempts = 3
retry_initial_backoff_ms = 100
```

## Dev commands
- Build: `just build`
- Run (SDK/CRT): `just run-sdk`, `just run-crt`, `just run-crt-slab`
- MinIO: `just minio-apply`, `just minio-port-forward`
- Upload helpers: `just mc-alias`, `just mc-mb`, `just mc-upload-ns`
- Manifest: `just manifest-gen ns path`
- Smoke tests: `just smoke-head`, `just smoke-stream`, `just smoke-demo`

## Docs
- [SPEC.md](docs/SPEC.md): Design spec
- [CRT.md](docs/CRT.md): CRT client notes & smoke tests
- [WORKLOG.md](docs/WORKLOG.md): Worklog & roadmap
- [TERMINOLOGY.md](docs/TERMINOLOGY.md): Terminology (namespace, page/segmentstore):

