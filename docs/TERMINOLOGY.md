# Terminology

This project uses the term "namespace" to describe a logical tenant prefix within an S3/MinIO bucket. It maps to a concrete key layout:

  s3://<bucket>/namespaces/{ns}/...

Within a namespace we store:
- HEAD (current manifest version pointer)
- manifests/v-{n}.json.gz (immutable, versioned)
- files/{path}/v-{hash}/... (optionally versioned by content hash)

Notes
- This is not equivalent to GPFS (IBM Spectrum Scale) "namespace". In GPFS, namespace usually refers to the filesystem's POSIX directory tree; multi-tenancy is often implemented via filesets with filesystem-level isolation, quotas, and snapshots. Here, a namespace is a prefix policy boundary with soft isolation (budgets, auth) and manifest-based versioning.
- Consistency: readers pin a manifest version (vN). Writers publish vN+1 and atomically swap HEAD.
- Tenancy: namespaces enable per-tenant GET/egress budgets and cache partitioning.

# Page and Segmentstore

- Page: fixed-size cached byte range (4–16 MiB) of a file. Historically this was also called a "slab"; you may still see "slab" in some code paths and feature flags. In user-facing docs and config we use "page".
- Segmentstore: disk-backed L2 that stores pages on the local filesystem (NVMe/SSD). In older docs/code this was called "slabfile"; the config accepts `backend = "segmentstore"` (preferred) or `backend = "slabfile"` (alias). The path key `segment_path` is preferred; `slabfile_path` remains as a backward-compatible alias.
- Default path for segmentstore: `/var/lib/seekfs/l2`.
