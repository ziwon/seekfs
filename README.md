# seekfs
Object‑Storage‑Native Training Data Plane (Rust)
> *“High‑throughput page‑slab cache for S3/MinIO — POSIX when needed, faster when not.”*

---

## v0.2 — What’s new

* **L2 cache backend options:** RocksDB+BlobDB **or** append‑only slabfile store with small RocksDB index to avoid LSM write‑amp for multi‑MiB values.
* **Integrity model:** per‑page **CRC32C** + file‑level **SHA‑256** in manifests; avoid relying on opaque ETags when SSE/KMS is enabled.
* **New high‑efficiency API:** `POST /readv` to batch many `(path,off,len)` into a single coalesced plan.
* **Hints v1 schema** and **advisor feedback** to auto‑tune prefetch value ("saved GETs").
* **Multi‑node topology:** consistent hashing for prewarm ownership + gossip for hot‑key announcements.
* **Write‑back journal** clarified for idempotent crash recovery and barrier grouping.
* **K8s deployment guidance:** hostPath NVMe persistence, node affinity, QoS, and warm‑cache survival.
* **Cost guardrails & reporting:** per‑namespace budgets and \$‑savings attribution.
* **Benchmark fairness:** s3fs default **and** tuned baselines; publish harness and CI.
* **Comparison section:** scope/ops contrasts vs **s3fs** and **3FS**.

---

## 0) Purpose & Positioning

**Goal:** Serve AI/ML training and evaluation with consistent, low‑tail‑latency reads and efficient checkpoint writes over **object storage as SSOT** (S3/MinIO), using an **intelligent cache** (RAM/SSD) and **Range GET** minimization.

* **Not a search engine.** Pair analytics with OLAP (ClickHouse/Trino/Spark). A light optional vector/search **sidecar** supports dataset discovery and drives prewarm **hints** (no impact on the main I/O plane).
* **Assumptions:** S3/MinIO provides strong R‑A‑W consistency for objects; manifests are immutable and versioned; compute is stateless.

---

## 1) Core Principles

1. **SSOT on object storage:** All durable data in S3/MinIO. Local SSD/RAM are cache only.
2. **Immutable manifests:** Versioned namespace manifests; jobs **pin a version** for reproducibility.
3. **Page‑slab I/O:** Fixed‑size slabs (4–16 MiB) + **page tables** → precise Range GETs; **coalesce** adjacent ranges.
4. **Read‑through + (optional) write‑back:** Async multipart uploads with a journal and **atomic** commit.
5. **Hit‑rate & tail first:** TinyLFU admission, ARC/LRU eviction, popularity prewarmers, hints‑driven prefetch.
6. **Stateless compute:** Cache is soft state; servers scale horizontally and can scale to zero.
7. **Bounded external calls:** Per‑node **S3 GET budgets** and **per‑namespace quotas**; global concurrency guards.

---

## 2) Architecture (Training Data Plane)

```
Client (Trainer / DataLoader / Tools)
        │           ┌──────────────────────────────────────┐
  (A) FUSE Mount    │  (B) Blob API (HTTP/gRPC) + READV   │
        │           └──────────────┬───────────────────────┘
        ▼                          │
┌──────────────────────────────────▼────────────────────────────────┐
│                        seekfs Cache Daemon                        │
│  • Manifest Loader (HEAD → vN)                                    │
│  • I/O Planner (path→(file version, slab ids, ranges), readv)     │
│  • L1 RAM Cache (TinyLFU + ARC/LRU)                               │
│  • L2 SSD Cache (RocksDB+BlobDB **or** slabfile+index)            │
│  • Prefetchers (sequential/stride) + Hints consumer (v1 schema)   │
│  • Upload Journal + Write‑Back Flusher (barrier groups)           │
│  • Hot‑Key Gossip + Consistent‑Hash Prewarm Ownership             │
└─────────────────────────┬─────────────────────────────────────────┘
                          ▼
                   S3 Range Fetcher (AWS CRT)
              • Coalesced Range GETs • Retries • Budgets
```

**Access modes**

* (A) **FUSE** for POSIX‑style reads/writes when frameworks expect files.
* (B) **Blob API** (faster, kernel‑bypass): `GET /blob`, **`POST /readv`**, `POST /hints`, `POST /barrier`, `PUT /upload`.

---

## 3) Storage & Metadata

### 3.1 Namespace layout (S3/MinIO)

```
s3://bucket/
└─ namespaces/{ns}/
   ├─ HEAD                          # current manifest version (tiny text)
   ├─ manifests/v-{n}.json.gz       # immutable manifest (gzip, cache‑forever)
   ├─ files/{path...}/v-{h}/...     # optional per‑file version folders (content hash)
   └─ segments/{id}.{meta,data}     # (internal if segmentizing larger files)
```

### 3.2 Manifest v1 (immutable, versioned)

```json
{
  "version": 42,
  "files": [
    {
      "path": "dataset/shard-00017.tar",
      "size": 1073741824,
      "hash": "sha256:...",         
      "page_table": [ { "page_id": 0, "off": 0, "len": 8388608, "crc32c": 1234567890 }, ... ],
      "storage": { "key": "files/dataset/shard-00017.tar", "etag": "...", "sse": "kms|none" }
    }
  ],
  "created_at": 1712345678,
  "parents": [41],
  "tombstones": []
}
```

**Notes**

* Use **file‑level SHA‑256** for versioning; per‑page **CRC32C** for fast corruption detection.
* ETags can be non‑deterministic with multipart + SSE/KMS; treat as **hint** only.

### 3.3 Rust types

```rust
#[derive(Serialize, Deserialize)]
pub struct PageInfo { pub page_id: u32, pub off: u64, pub len: u32, pub crc32c: u32 }

#[derive(Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub hash: String,           // sha256 content hash (versioning)
    pub page_table: Vec<PageInfo>,
    pub storage: ObjectLoc,     // S3 key + etag + sse mode
}

#[derive(Serialize, Deserialize)]
pub struct Manifest {
    pub version: u64,
    pub files: Vec<FileEntry>,
    pub parents: Vec<u64>,
    pub tombstones: Vec<String>,
    pub created_at: u64,
}
```

---

## 4) Interfaces

### 4.1 FUSE (initially RO; RW with write‑back)

* **Read path:** map `(path, offset, len)` to page ids → L1/L2 lookup → miss → Range GET.
* **Write path (opt‑in):** buffer to slabs → multipart upload; on commit, publish new manifest `vN+1` and atomically swap `HEAD` (`If‑Match`).

### 4.2 Blob API (HTTP/gRPC)

* `GET /blob?path=&off=&len=` → bytes (zero‑copy to NIC where possible).
* **`POST /readv`** body: `[{ "path": "...", "off": 0, "len": 8388608 }, ...]` → batched read; **single coalesced plan** per file.
* `POST /hints` (v1) body:

```json
{
  "epoch": 12,
  "priority": "normal|high",
  "windows": [
    { "path": "dataset/shard-00017.tar", "ranges": [[0,16777216],[33554432,16777216]] }
  ],
  "ttl_ms": 60000
}
```

* `POST /barrier` → ensure all prior writes in the journal are **durable** (for checkpoints); returns a barrier id.
* `PUT /upload?path=` (stream) → write‑back with journal; returns new file hash/id.
* `GET /metrics` → Prometheus; `GET /advisor` → top warm candidates & saved‑GETs per hint source.

---

## 5) Cache Design

* **Slab size:** 4–16 MiB (default 8 MiB); align with parquet row‑groups/webdataset samples.
* **L1 (RAM):** lock‑free/low‑contention index + **TinyLFU admission** + ARC/LRU eviction.
* **L2 (SSD) — backends:**

  1. **RocksDB+BlobDB**: store keys and small metadata in LSM; large values in blob files.
  2. **Slabfile+Index**: append‑only slabfiles on NVMe; tiny RocksDB index → (ns,path,hash,page\_id) → `(file_id, slab_off,len)`.

  * Pick via config; both expose the same trait: `L2::get/put/delete/iter_stats`.
* **Duplicate‑fetch suppression:** in‑flight de‑dup keyed by `(ns,path,hash,page_id)`.
* **Prefetchers:**

  * **Sequential** (prefetch N slabs ahead), **Stride** (detect fixed gaps), **Hints v1** (trainer‑driven), with feedback loop: compute **saved‑GETs** per hint source and auto‑adjust.
* **Multi‑node cooperation:**

  * **Consistent hashing** assigns prewarm ownership to nodes; **gossip** broadcasts hot keys & hit deltas to reduce duplicate cold pulls.
* **Coherency:** keys include **file\_hash**; manifest pinning guarantees per‑job consistency.

---

## 6) Consistency & Semantics

* **Readers:** read `HEAD` → fetch `vN` → **pin version** for job lifetime; cache may be stale vs HEAD but is **consistent within vN**.
* **Writers:** upload to staging keys → produce `vN+1` → atomic **HEAD swap** with `If‑Match`.
* **Leases/locks:** optional short‑TTL lease for active writers per `path`.
* **Barriers & atomic groups:** `POST /barrier` groups checkpoint files; swap manifest once all are durable.
* **Crash recovery (write‑back):** journal is idempotent: (upload part → committed set → manifest write → `HEAD` swap). On restart, re‑list parts; GC orphans by age.
* **Rename/atomicity:** renames materialize in the new manifest; readers see changes only after `HEAD` swap.

---

## 7) S3/MinIO I/O Control

* **AWS CRT client** for high‑throughput Range GETs and multipart PUTs.
* **Per‑node GET budget** (e.g., 200 GET/s) + **per‑namespace quotas** for GET/s and egress.
* **In‑flight caps:** global semaphore for concurrent S3 ops.
* **Range coalescing:** merge adjacent reads up to `range_coalesce_max_mib` (default 32 MiB).
* **Retries:** exp. backoff + jitter; circuit‑break on hot prefixes; **key dispersion** via randomized prefixes.

---

## 8) Observability

* **Metrics:** bytes from cache vs S3, **GETs/read**, L1/L2 hit‑rate, P50/P95/P99 (8–16 MiB reads), concurrency, eviction causes, write‑back backlog.
* **Cost view:** per‑namespace **\$ avoided** from GET reduction and egress savings; export **saved‑GETs/GB** per job.
* **Tracing (OTel):** spans: manifest load → plan → L1/L2 → S3 fetch → return; annotate `(ns, path, file_hash, page_ids, manifest_version)`.
* **Advisor:** surfacing “top warm candidates,” “worst hotspots,” and “hint ROI.”

---

## 9) Security & Tenancy

* **IAM‑scoped prefixes;** optional SSE‑KMS with tenant context.
* **Per‑namespace quotas & auth:** API tokens or mTLS for Blob API; FUSE mounts whitelisted per node.
* **Audit:** log manifest swaps, barrier commits, and administrative overrides.

---

## 10) Deployment (Kubernetes & Bare‑Metal)

* **K8s DaemonSet**: one daemon per node; **hostPath NVMe** for L2 so caches survive pod restarts.
* **Node‑affinity / anti‑affinity:** keep workloads near their warm caches; optional topology spread.
* **QoS & throttling:** dedicate CPU shares and IO classes; protect trainers from cache storms.
* **Warm‑start:** restore L2 index on boot; delay eviction until health & budgets are confirmed.

---

## 11) Performance Targets

| Metric                       | Target                          | Notes                                 |
| ---------------------------- | ------------------------------- | ------------------------------------- |
| **Random read P95 (warm)**   | ≥ **2×** s3fs (default & tuned) | 8–16 MiB reads; same host/AZ/endpoint |
| **Random read P95 (cold)**   | ≥ **1.3×** s3fs                 | coalescing & fewer GETs               |
| **Sequential throughput**    | ≥ s3fs                          | lower P99/P50 ratio (tighter tails)   |
| **GETs/read**                | ≤ **0.5×** s3fs                 | with slab coalescing & hints          |
| **Checkpoint write (async)** | ≥ **1.5×** s3fs                 | at same durability barrier            |

*Publish P50/P95/P99 and CPU/RSS for both **s3fs default** and **s3fs tuned** profiles.*

---

## 12) Benchmark Plan vs s3fs

**Workloads**

1. **Sequential shards:** 10–100 GB webdataset `.tar` or parquet (RG 128–256 MiB).
2. **Random reads:** 1k–100k × (8–16 MiB) from random offsets.
3. **Mixed parallel:** N=8/16/32 workers doing (2).
4. **Checkpoint write‑back:** 20–80 GB single file (multipart).
5. **Warm epoch repeat:** re‑run (1–3) with cache warm.

**Methodology**

* Same host/AZ/endpoint; pin manifest version; 5 runs each; report median & 95% CI.
* Compare **s3fs default** and **s3fs tuned** (document flags like `-o parallel_count`, read‑ahead, multipart thresholds, etc.).
* Record: GETs, bytes from S3 vs cache, latencies, CPU/RSS, retries, cost deltas.
* Provide a **repro harness** (container + scripts) and CI results.

---

## 13) Configuration (example)

```toml
# seekfs.toml
[cache]
slab_size_mib = 8
l1_memory_gib = 32
l2_ssd_gib = 512
admission = "tinylfu"

[cache.l2]
# backend = "blobdb" | "slabfile"
backend = "slabfile"
slabfile_path = "/nvme/seekfs/slabs"

[s3]
get_budget_per_sec = 200
range_coalesce_max_mib = 32
max_inflight_gets = 128
endpoint = "https://s3.example.com"

[prefetch]
sequential = true
prefetch_distance = 3
stride_bytes = 4_194_304
hints_endpoint = true

[writeback]
mode = "async"        # async | sync | disabled
flush_interval_ms = 500
max_inflight_parts = 256
barrier_timeout_ms = 60000

[consistency]
pin_manifest_version = "v42"
lease_ttl_sec = 300

[tenancy]
ns_get_budget_per_sec = 120
ns_egress_mib_per_sec = 512

[api]
listen = "0.0.0.0:7070"
mtls = false
```

---

## 14) Repository & Crates

```
seekfs/
├─ crates/
│  ├─ seekfs-core       # manifests, planner, L1/L2 cache trait, S3 (CRT)
│  ├─ seekfs-l2         # L2 backends: blobdb | slabfile
│  ├─ seekfs-fuse       # FUSE client (POSIX mount)
│  ├─ seekfs-daemon     # cache daemon + Blob API + hints/barrier/readv
│  ├─ seekfs-bench      # microbench + scenarios (s3fs parity tests)
│  ├─ seekfs-proto      # gRPC/HTTP types (Blob API, hints v1, advisor)
│  └─ seekfs-tools      # warmers, prefetch planners, upload‑GC, ctl
├─ deploy/              # systemd units, k8s DaemonSet/Helm, Dockerfiles
├─ docs/                # design notes, tuning guides, bench reports
└─ README.md
```

---

## 15) Roadmap (PoC → GA)

* **P0**: Manifest v1 + HEAD swap; AWS CRT path; slab cache (L1 + L2 backend skeleton); GET budgets; metrics skeleton.
* **P1**: FUSE‑RO + Blob `GET` + **`readv`**; sequential/stride prefetch; full metrics/tracing; duplicate‑fetch suppression.
* **P2**: Hints v1 API + advisor feedback; benchmark suite vs s3fs (default & tuned) with repro harness; publish results.
* **P3**: Write‑back (multipart) + barriers; leases; GC of orphan parts; crash‑safe journal.
* **P4**: Multi‑node: consistent‑hash prewarm, hot‑key gossip; K8s DaemonSet hardening; cost guardrails & reports.
* **P5**: Optional NFS‑Ganesha export; metadata/search sidecar (IVF‑PQ/HNSW) powering hints; operator/auto‑tuning.

---

## 16) Comparison & Positioning

**seekfs vs s3fs**

* **Scope**: seekfs is an object‑store‑native cache with explicit manifests, budgets, and prefetch/hints; s3fs is a general FUSE for S3.
* **Perf**: seekfs targets lower tail latency and fewer GETs via slab coalescing and admission; s3fs largely relies on kernel readahead.
* **Repro**: seekfs pins **manifest versions** per job; s3fs exposes current object state without job‑level pinning.

**seekfs vs 3FS**

* **Scope**: seekfs layers on top of S3/MinIO (no new storage cluster); 3FS is a distributed FS with SSD+RDMA as primary fabric.
* **Ops**: seekfs fits existing S3 infra; 3FS requires operating its own high‑perf storage network.
* **Fit**: For S3‑resident datasets and elastic trainers, seekfs is lighter; 3FS shines when you can deploy/operate its cluster and need shared primary storage.

---

## 17) Risks & Mitigations

* **LSM write‑amp with MiB values** → use BlobDB or slabfile backends; keep LSM keys small.
* **Hint misprediction** → compute ROI (saved‑GETs) per source; decay or disable low‑value hints.
* **Hot‑prefix throttling by object store** → randomized prefixes, budgets, and backoff/circuit‑breakers.
* **Cache storms on node restart** → warm‑start L2, cap in‑flight GETs, prewarm via consistent hashing.
* **Multipart + SSE/KMS ETag variance** → rely on SHA‑256 + CRC32C, not ETag, for correctness.

---

## 18) Optional: Metadata/Search Sidecar (later)

For curated subsets and hard‑negative mining: small IVF‑PQ/HNSW index over **metadata/embeddings**; returns file paths/byte ranges to **seekfs hints** for prewarm (no coupling to main I/O path).

---

## 19) Glossary

* **Slab**: fixed‑size cached byte range (4–16 MiB) of a file.
* **readv**: batched read interface that plans/coalesces multiple ranges efficiently.
* **Barrier**: durability checkpoint that groups writes across files before a manifest swap.
* **Saved‑GETs**: GET operations avoided due to cache/prefetch/hints, used for cost and ROI reporting.
