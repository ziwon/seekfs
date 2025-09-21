# Worklog / Implementation Summary

This document summarizes the major tasks completed so far, with key files and notes.

## Workspace & Scaffolding
- Rust workspace with crates:
  - crates/seekfs-core: core types (Manifest, PageInfo, CacheKey), L2 trait, ObjectStore trait.
  - crates/seekfs-l2: L2 backends (in-memory; segmentstore/slabfile with index).
  - crates/seekfs-daemon: Axum-based HTTP daemon (Blob API, manifest loader, readv).
  - crates/seekfs-proto: JSON types (readv, hints, advisor).
  - crates/seekfs-tools: CLI tools (seekfs-ctl), incl. manifest generator.
  - crates/seekfs-bench: bench scaffold.
- Justfile: common developer tasks (build/run, MinIO install via Helm, smoke tests, mc helpers).

## Object Store (S3/MinIO)
- Abstraction: `seekfs_core::store::ObjectStore` with SDK (default) and CRT (feature `crt`).
- SDK store: aws-sdk-s3 with path-style addressing; budgets/inflight caps; streaming support.
- CRT store: based on `mountpoint-s3-client` (AWS CRT), optional feature `crt`.
- Config: `[s3] { engine = "sdk" | "crt", endpoint, region, bucket, access_key_id, secret_access_key, budgets }`.

## Manifest & Namespace
- Namespace-aligned S3 layout helpers in `seekfs_core::layout`:
  - `manifest_head_key(ns)`, `manifest_version_key(ns, v)`, `file_key(ns, path, hash?)`.
- Manifest loader/cache in daemon (`crates/seekfs-daemon/src/manifest.rs`):
  - Loads `namespaces/{ns}/HEAD`, fetches and gunzips `manifests/v-{n}.json.gz`.
  - 10s TTL cache, helper to find FileEntry and retrieve per-page CRC.
- CLI manifest generator (`seekfs-ctl gen-manifest`):
  - Computes SHA-256 and CRC32C per page, writes `manifests/v-N.json.gz`, updates `HEAD`.

## Blob API & Read Path
- Endpoints in daemon:
  - `GET /healthz`, `GET /advisor` (stub).
  - `GET /test_head`, `GET /test_stream` (dev helpers).
  - `GET /blob?ns=&path=&off=&len[&hash=]`:
    - Resolves manifest; streams missing pages from S3 → validates CRC32C → writes to L2; returns requested bytes.
  - `POST /readv?ns=` with body `[{path,off,len}, ...]`:
    - Groups by path; ensures missing pages in L2; concatenates requested ranges in order.
- Page CRC32C verification:
  - On first download from S3, buffer page, validate against manifest `page_table`, then persist to L2.

## L2 Cache (segmentstore)
- In-memory backend (feature `mem`): simple HashMap; supports `put_stream`.
- Disk-backed segmentstore (feature `slabfile`, production name “segmentstore”):
  - Default path `/var/lib/seekfs/l2` (configurable via `[cache.l2].segment_path`, alias: `slabfile_path`).
  - Append-only segments `data-00001.seg` (roll at 1 GiB).
  - Index DB (`redb`) maps `(ns|path|file_hash|page_id)` hash → `(segment_id, offset, len)`.
  - Backward compatibility: falls back to hashed-per-page files if no index entry exists.
  - Commit path uses atomic append + index update; delete removes index entry (space reclaimed later).

## MinIO Dev Environment
- Helm-based MinIO install targets in Justfile (`minio-apply`, `minio-apply-pv`, `minio-port-forward`).
- Namespace `object`, port-forward to 9000/9001.
- mc helpers (`mc-alias`, `mc-mb`, `mc-upload`, `mc-upload-ns`).

## Developer UX
- Just targets: `build`, `run-sdk`, `run-crt`, `run-crt-slab`, `smoke-head`, `smoke-stream`, `smoke-demo`, `manifest-gen`, MinIO setup.
- Config defaults updated to segmentstore/page vocabulary.

## Docs
- Converted README to a practical, user-oriented overview.
- Moved the detailed design spec to `docs/SPEC.md`.
- Clarified terminology in `docs/TERMINOLOGY.md` (namespace vs GPFS, page vs slab, segmentstore vs slabfile).
- Updated `docs/CRT.md` usage examples.

## Next
- Segmentstore compaction/GC and (optional) alternate index backend (e.g., RocksDB).
- Add metrics (/metrics) for GETs, L2 hits/misses, CRC failures, segment bytes, index entries.
- FUSE (RO) over Blob API after data plane hardening.

