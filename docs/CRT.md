AWS CRT client (C3) integration

Overview
- The `crt` feature wires a real AWS Common Runtime S3 client via `mountpoint-s3-client`.
- It implements the same `ObjectStore` trait as the SDK path. Reads use HTTP Range GET; HEAD is approximated via a 0..1 GET and Content-Range.

Crate features
- Default: SDK client only (`seekfs-core` feature `sdk`).
- Enable CRT: build with `--features crt` (on `seekfs-core` and any binary depending on it, e.g., `seekfs-daemon`).

System prerequisites (for CRT)
- `cmake`, `gcc`/`g++`, `make` are required to build the AWS CRT native libs used by `mountpoint-s3-crt-sys`.
- On Debian/Ubuntu: `sudo apt-get update && sudo apt-get install -y cmake build-essential`

Daemon usage
- Build: `cargo run -p seekfs-daemon --features crt`
- Config (`seekfs.toml`):
  - `[s3] engine = "crt"`
  - Provide `endpoint`, `region`, `bucket`, `access_key_id`, `secret_access_key` for MinIO.
- Smoke tests:
  - Range GET stream to L2 (segmentstore): `curl 'http://127.0.0.1:7070/test_stream?key=some-object&off=0&len=1024'`
  - HEAD: `curl 'http://127.0.0.1:7070/test_head?key=some-object'`
  - Blob: `curl 'http://127.0.0.1:7070/blob?ns=dev&path=warm.bin&off=0&len=4096'`

Segmentstore vs slabfile
- The disk-backed L2 is referred to as “segmentstore” in config/docs; the older name “slabfile” is kept as a feature alias in code.

Notes
- MinIO: path-style addressing is used by the CRT client through `mountpoint-s3-client` when you set `with_endpoint()` to an HTTP URL.
- HEAD behavior: for portability and simplicity, `head()` performs a 0..1 Range GET and parses `Content-Range`.
- Budgets/concurrency: the same token-bucket and inflight semaphores are applied around the CRT client as with the SDK path.
