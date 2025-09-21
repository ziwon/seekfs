# seekfs — Design Specification (snapshot)

This document describes purpose, architecture, storage layout, APIs, cache design, consistency, performance targets, configuration, repository layout, and roadmap.

Note: In docs and config, we adopt “page/segmentstore” terminology (formerly referred to as “slab/slabfile”). The alias “slabfile” remains supported in code and config for now.

## 0) Purpose & Positioning

Goal: Serve AI/ML training and evaluation with consistent, low‑tail‑latency reads and efficient checkpoint writes over object storage (S3/MinIO) as SSOT, using an intelligent cache (RAM/SSD) and Range GET minimization.

Assumptions: S3/MinIO provides strong read‑after‑write consistency; manifests are immutable and versioned; compute is stateless.

## 1) Core Principles
1. SSOT on object storage; local disks are cache only.
2. Immutable manifests (versioned); jobs pin a version.
3. Page‑based I/O (4–16 MiB) with page tables; precise Range GETs; coalescing.
4. Read‑through + optional write‑back; atomic commit.
5. Tail‑latency focus; TinyLFU admission; adaptive prefetch.
6. Stateless compute; horizontal scaling.
7. Bounded external calls; budgets/quotas.

## 2) Architecture (Training Data Plane)

Client (FUSE mount or Blob API) → seekfs daemon → S3 Range Fetcher (AWS SDK/CRT).

Daemon components:
- Manifest loader (HEAD → vN)
- I/O planner (path → (file version, page ids, ranges))
- L1 RAM cache (TinyLFU + ARC/LRU)
- L2 SSD cache: RocksDB+BlobDB or segmentstore+index
- Prefetchers + hints consumer
- Upload journal + write‑back flusher
- Hot‑key gossip + consistent‑hash prewarm ownership

## 3) Storage & Metadata

Namespace layout:

```
s3://bucket/
└─ namespaces/{ns}/
   ├─ HEAD                          # current manifest version (tiny text)
   ├─ manifests/v-{n}.json.gz       # immutable manifest (gzip, cache‑forever)
   └─ files/{path...}/...           # optional per‑file version folders (content hash)
```

Manifest v1:

```
version, files: [{ path, size, hash, page_table[{page_id,off,len,crc32c}], storage{key,etag,sse} }], created_at, parents, tombstones
```

Integrity: file‑level SHA‑256 (versioning), per‑page CRC32C (corruption detection). ETag considered a hint only.

## 4) Interfaces

FUSE (RO first; RW with write‑back). Blob API (HTTP/gRPC):
- GET /blob?ns=&path=&off=&len[&hash=]
- POST /readv?ns= — body: [{ path, off, len }, ...]
- POST /hints, POST /barrier, PUT /upload
- /metrics, /advisor

## 5) Cache Design

Page size: 4–16 MiB (default 8 MiB). L1: TinyLFU+ARC/LRU. L2 backends:
1) RocksDB+BlobDB (large values in blob files)
2) Segmentstore+Index (aka slabfile): append‑only page segments on NVMe; a small embedded index maps (ns,path,hash,page_id) → (segment_id, offset, len).

Duplicate‑fetch suppression; prefetchers; multi‑node cooperation. Coherency via file_hash in keys; manifest pinning assures per‑job consistency.

## 6) Consistency & Semantics

Readers pin vN. Writers upload to staging → publish vN+1 → atomic HEAD swap. Optional leases/locks. Barriers group checkpoint writes. Crash recovery is idempotent. Rename manifests in the next version.

## 7) S3/MinIO I/O Control

AWS CRT client recommended for high‑throughput Range GETs and multipart PUTs. GET budgets/quotas; inflight caps; coalescing; retries; prefix dispersion.

## 8) Observability

Metrics (bytes, GETs, hit‑rates, latency), cost view (saved‑GETs), distributed tracing, advisor.

## 9) Security & Tenancy

IAM‑scoped prefixes; optional SSE‑KMS; per‑namespace quotas/auth; audit logs.

## 10) Deployment

K8s DaemonSet per node (NVMe hostPath); affinity/anti‑affinity; QoS/throttling; warm‑start L2 index.

## 11) Performance Targets

Random read P95 (warm) ≥ 2× s3fs; GETs/read ≤ 0.5× s3fs via page‑aligned coalescing & hints; publish P50/P95/P99 and CPU/RSS.

## 12) Benchmark Plan vs s3fs

Workloads: sequential shards, random reads, mixed parallel, checkpoint write‑back, warm epoch repeat. Methodology: same host/AZ, pinned manifest, 5 runs, CI harness.

## 13) Configuration (example)

```toml
[cache]
slab_size_mib = 8
l1_memory_gib = 32

[cache.l2]
backend = "segmentstore"              # alias: "slabfile"
segment_path = "/var/lib/seekfs/l2"

[s3]
engine = "sdk"                         # sdk | crt
endpoint = "https://s3.example.com"
get_budget_per_sec = 200
range_coalesce_max_mib = 32
max_inflight_gets = 128

[api]
listen = "0.0.0.0:7070"
```

## 14) Repository & Crates

seekfs-core (types/traits), seekfs-l2 (L2 backends: blobdb | segmentstore), seekfs-daemon (Blob API + manifest), seekfs-tools (ctl), seekfs-proto, seekfs-fuse, seekfs-bench, deploy/, docs/.

## 15) Roadmap (PoC → GA)

- P0: Manifest v1 + HEAD swap; CRT path; page cache skeleton; budgets; metrics skeleton.
- P1: FUSE‑RO + Blob GET + readv; prefetch; tracing; dedupe.
- P2: Hints v1 + advisor; benchmark suite vs s3fs.
- P3: Write‑back (multipart) + barriers; leases; GC; crash‑safe journal.
- P4: Multi‑node prewarm + gossip; K8s hardening; cost guardrails.
- P5: Optional NFS‑Ganesha; metadata/search sidecar; operator/auto‑tuning.

## 16) Comparison & Positioning

seekfs vs s3fs: fewer GETs via page‑aligned coalescing/hints; manifest pinning per job. seekfs vs 3FS: seekfs layers on S3/MinIO; 3FS is primary storage with its own fabric.

## 19) Glossary

- Page: fixed‑size cached range (4–16 MiB). 
- Segmentstore: disk L2 (alias: slabfile). 
- Namespace: tenant prefix (not the same as GPFS namespace/fileset).

