use axum::{extract::{State, Query}, http::{StatusCode, header}, routing::{get, post}, Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tracing::{error, info, Level};

use futures::StreamExt;
use seekfs_core::{l2::{L2Backend, L2Put}, store::{S3Config, ObjectStore, SdkStore}, layout::file_key, Manifest};
use seekfs_l2::MemL2;
use seekfs_proto::{AdvisorResponse, HintsRequest, ReadvItem};
mod manifest;
use manifest::{ManifestCache, find_file, page_crc_by_id};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheCfg {
    #[serde(default = "default_slab_size_mib")] 
    slab_size_mib: u32,
    #[serde(default)]
    l2: Option<L2Cfg>,
}

fn default_slab_size_mib() -> u32 { 8 }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApiCfg {
    #[serde(default = "default_listen")] 
    listen: String,
}

fn default_listen() -> String { "0.0.0.0:7070".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct L2Cfg {
    #[serde(default = "default_l2_backend")] 
    backend: String, // mem | slabfile | blobdb
    #[serde(default)]
    slabfile_path: Option<String>,
    #[serde(default)]
    segment_path: Option<String>,
}

fn default_l2_backend() -> String { "mem".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    cache: Option<CacheCfg>,
    #[serde(default)]
    api: Option<ApiCfg>,
    #[serde(default)]
    s3: Option<S3Config>,
}

struct AppState {
    l2: Arc<dyn L2Backend>,
    store: Option<Arc<dyn ObjectStore>>, // sdk or crt
    manifests: ManifestCache,
    slab_size: u64,
}

#[tokio::main]
async fn main() {
    init_tracing();

    let cfg_path = std::env::var("SEEKFS_CONFIG").ok().map(PathBuf::from);
    let cfg = match cfg_path {
        Some(p) => match std::fs::read_to_string(&p) {
            Ok(s) => match toml::from_str::<Config>(&s) { Ok(c) => c, Err(e) => { error!(?e, "bad config TOML"); Default::default() } },
            Err(e) => { error!(?e, "failed to read config"); Default::default() }
        },
        None => Default::default(),
    };

    let listen_addr: SocketAddr = cfg.api.as_ref().map(|a| a.listen.as_str()).unwrap_or("0.0.0.0:7070").parse().expect("bad listen addr");

    // Initialize ObjectStore (engine = sdk|crt) if configured
    let store: Option<Arc<dyn ObjectStore>> = if let Some(s3_cfg) = cfg.s3.clone() {
        match s3_cfg.engine.as_deref().unwrap_or("sdk") {
            "sdk" => match SdkStore::new(s3_cfg.clone()).await {
                Ok(s) => { info!("ObjectStore=SDK initialized (bucket: {})", s3_cfg.bucket); Some(Arc::new(s) as Arc<dyn ObjectStore>) }
                Err(e) => { error!("Failed to init SDK store: {}", e); None }
            },
            #[cfg(feature = "crt")]
            "crt" => {
                use seekfs_core::store::crt_impl::CrtStore;
                match CrtStore::new(s3_cfg.clone()).await {
                    Ok(s) => { info!("ObjectStore=CRT initialized (bucket: {})", s3_cfg.bucket); Some(Arc::new(s) as Arc<dyn ObjectStore>) }
                    Err(e) => { error!("Failed to init CRT store: {}", e); None }
                }
            }
            other => { error!("Unknown s3.engine '{}', skipping store init", other); None }
        }
    } else { info!("No S3 config; running without ObjectStore"); None };

    // Select L2 backend from config (default: mem)
    let l2: Arc<dyn L2Backend> = match cfg.cache.as_ref().and_then(|c| c.l2.as_ref()).map(|l2| l2.backend.as_str()) {
        Some("mem") | None => Arc::new(MemL2::new()),
        Some("slabfile") | Some("segmentstore") => {
            #[cfg(feature = "slabfile")]
            {
                let base = cfg.cache.as_ref()
                    .and_then(|c| c.l2.as_ref())
                    .and_then(|l2| l2.segment_path.clone().or(l2.slabfile_path.clone()))
                    .unwrap_or_else(|| "/var/lib/seekfs/l2".to_string());
                match seekfs_l2::SlabfileL2::new(&base) {
                    Ok(b) => {
                        tracing::info!(path=%base, "Using L2 backend segmentstore");
                        Arc::new(b) as Arc<dyn L2Backend>
                    }
                    Err(e) => {
                        tracing::error!(%e, "Failed to init slabfile L2; falling back to mem");
                        Arc::new(MemL2::new())
                    }
                }
            }
            #[cfg(not(feature = "slabfile"))]
            {
                tracing::warn!("L2 backend 'segmentstore' feature not enabled; falling back to 'mem'");
                Arc::new(MemL2::new())
            }
        }
        Some("blobdb") => {
            tracing::warn!("L2 backend 'blobdb' not implemented yet, falling back to 'mem'");
            Arc::new(MemL2::new())
        }
        Some(other) => {
            tracing::warn!(backend = %other, "Unknown L2 backend, falling back to 'mem'");
            Arc::new(MemL2::new())
        }
    };

    let slab_size = (cfg.cache.as_ref().and_then(|c| Some(c.slab_size_mib)).unwrap_or(8) as u64) * 1024 * 1024;
    let state = Arc::new(AppState { l2, store, manifests: ManifestCache::new(std::time::Duration::from_secs(10)), slab_size });

    let app = Router::new()
        .route("/healthz", get(health))
        .route("/metrics", get(metrics))
        .route("/advisor", get(advisor))
        .route("/readv", post(readv))
        .route("/hints", post(hints))
        .route("/test_s3", get(test_s3))
        .route("/test_stream", get(test_stream_to_l2))
        .route("/test_head", get(test_head))
        .route("/blob", get(get_blob))
        .with_state(state);

    info!(addr = %listen_addr, "seekfs-daemon listening");
    axum::serve(tokio::net::TcpListener::bind(listen_addr).await.unwrap(), app)
        .await
        .unwrap();
}

fn init_tracing() {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,hyper=warn,axum::rejection=trace".to_string());
    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_env_filter(env_filter)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

async fn health() -> &'static str { "ok" }
async fn metrics() -> &'static str { "# seekfs metrics\n" }

async fn advisor() -> Json<AdvisorResponse> {
    Json(AdvisorResponse { top_warm_candidates: vec![] })
}

#[derive(Debug, serde::Deserialize)]
struct ReadvQuery { ns: String }

async fn readv(State(state): State<Arc<AppState>>, Query(q): Query<ReadvQuery>, Json(items): Json<Vec<ReadvItem>>) -> Result<(StatusCode, axum::http::HeaderMap, Bytes), (StatusCode, String)> {
    if items.is_empty() { return Err((StatusCode::BAD_REQUEST, "empty readv".into())); }
    let store = state.store.clone().ok_or((StatusCode::SERVICE_UNAVAILABLE, "ObjectStore not configured".to_string()))?;
    // Group by path
    use std::collections::{BTreeMap, BTreeSet};
    let mut by_path: BTreeMap<String, Vec<ReadvItem>> = BTreeMap::new();
    let mut total_len: usize = 0;
    for it in items.into_iter() {
        total_len = total_len.saturating_add(it.len as usize);
        by_path.entry(it.path.clone()).or_default().push(it);
    }

    let slab = state.slab_size;
    // For each path, ensure needed slabs are present
    let manif = state.manifests.get_or_load(store.clone(), &q.ns).await.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("manifest load: {e}")))?;
    for (path, reqs) in by_path.iter() {
        let fe = find_file(&manif.manifest, path).ok_or((StatusCode::NOT_FOUND, format!("file not in manifest: {}", path)))?;
        let storage_key = fe.storage.key.clone();
        let mut needed: BTreeSet<u32> = BTreeSet::new();
        for r in reqs.iter() {
            if r.off >= fe.size { return Err((StatusCode::RANGE_NOT_SATISFIABLE, format!("{}: offset beyond EOF", path))); }
            let end = r.off.saturating_add(r.len as u64).min(fe.size);
            let first = (r.off / slab) as u32;
            let last = ((end - 1) / slab) as u32;
            for pid in first..=last { needed.insert(pid); }
        }
        for pid in needed {
            let ck = seekfs_core::types::CacheKey {
                ns: seekfs_core::types::Namespace(q.ns.clone()),
                item: seekfs_core::types::PathAndVersion { path: path.clone(), file_hash: fe.hash.clone() },
                page_id: pid,
            };
            if state.l2.get(&ck).is_none() {
                let start = (pid as u64) * slab;
                let mut len = slab;
                if start + len > fe.size { len = fe.size - start; }
                let mut stream = store.get_range_stream(&storage_key, start..start+len)
                    .await.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("store stream error: {e}")))?;
                let mut page_buf: Vec<u8> = Vec::with_capacity(len as usize);
                while let Some(chunk) = stream.next().await {
                    let b = chunk.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("store chunk: {e}")))?;
                    page_buf.extend_from_slice(&b);
                }
                if let Some(exp) = page_crc_by_id(fe, pid) {
                    let got = crc32c::crc32c(&page_buf);
                    if got != exp {
                        return Err((StatusCode::SERVICE_UNAVAILABLE, format!("crc32c mismatch for page {}: got {}, expected {}", pid, got, exp)));
                    }
                }
                let mut sink = state.l2.put_stream(ck, Some(page_buf.len()))
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 put_stream error: {e}")))?;
                sink.write(&page_buf).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 write: {e}")))?;
                let _ = sink.commit().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 commit: {e}")))?;
            }
        }
    }

    // Build response by concatenating requested ranges in order
    let mut out = bytes::BytesMut::with_capacity(total_len);
    for (path, reqs) in by_path.iter() {
        let fe = find_file(&manif.manifest, path).unwrap();
        for r in reqs.iter() {
            let end = r.off.saturating_add(r.len as u64).min(fe.size);
            let mut cur_off = r.off;
            while cur_off < end {
                let pid = (cur_off / slab) as u32;
                let ck = seekfs_core::types::CacheKey {
                    ns: seekfs_core::types::Namespace(q.ns.clone()),
                    item: seekfs_core::types::PathAndVersion { path: path.clone(), file_hash: fe.hash.clone() },
                    page_id: pid,
                };
                let slab_bytes = state.l2.get(&ck).ok_or((StatusCode::INTERNAL_SERVER_ERROR, "l2 missing after fetch".to_string()))?;
                let slab_start = (pid as u64) * slab;
                let from = if cur_off > slab_start { (cur_off - slab_start) as usize } else { 0 };
                let max_to = ((end - slab_start) as usize).min(slab_bytes.len());
                out.extend_from_slice(&slab_bytes[from..max_to]);
                cur_off = slab_start + max_to as u64;
            }
        }
    }
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from_str(&out.len().to_string()).unwrap());
    Ok((StatusCode::OK, headers, out.freeze()))
}

async fn hints(State(_state): State<Arc<AppState>>, Json(_h): Json<HintsRequest>) -> StatusCode { StatusCode::ACCEPTED }

#[derive(Debug, serde::Deserialize)]
struct TestQuery { key: Option<String>, ns: Option<String>, path: Option<String>, hash: Option<String>, len: Option<u64>, off: Option<u64> }

async fn test_s3(State(state): State<Arc<AppState>>, Query(q): Query<TestQuery>) -> (StatusCode, String) {
    let Some(store) = &state.store else { return (StatusCode::SERVICE_UNAVAILABLE, "ObjectStore not configured".into()); };
    let key_buf;
    let key = if let Some(k) = q.key.as_deref() { k } else {
        let ns = q.ns.as_deref().unwrap_or("dev");
        let path = q.path.as_deref().unwrap_or("test-file");
        key_buf = file_key(ns, path, q.hash.as_deref());
        &key_buf
    };
    let len = q.len.unwrap_or(1024);
    let off = q.off.unwrap_or(0);
    match store.get_range(key, off..off+len).await {
        Ok(bytes) => (StatusCode::OK, format!("store={}, fetched {} bytes", store.name(), bytes.len())),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("fetch failed: {}", e)),
    }
}

#[derive(Debug, serde::Deserialize)]
struct HeadQuery { key: Option<String>, ns: Option<String>, path: Option<String>, hash: Option<String> }

async fn test_head(State(state): State<Arc<AppState>>, Query(q): Query<HeadQuery>) -> (StatusCode, String) {
    let Some(store) = &state.store else { return (StatusCode::SERVICE_UNAVAILABLE, "ObjectStore not configured".into()); };
    let key_buf;
    let key = if let Some(k) = q.key.as_deref() { k } else {
        let ns = q.ns.as_deref().unwrap_or("dev");
        let path = q.path.as_deref().unwrap_or("");
        key_buf = file_key(ns, path, q.hash.as_deref());
        &key_buf
    };
    match store.head(key).await {
        Ok(Some(info)) => (StatusCode::OK, format!("size={:?} etag={:?}", info.size, info.etag)),
        Ok(None) => (StatusCode::NOT_FOUND, "not found".into()),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, format!("head failed: {e}")),
    }
}

#[derive(Debug, serde::Deserialize)]
struct StreamQuery {
    key: String,
    off: u64,
    len: u64,
    ns: Option<String>,
    path: Option<String>,
    hash: Option<String>,
    page_id: Option<u32>,
}

async fn test_stream_to_l2(State(state): State<Arc<AppState>>, Query(q): Query<StreamQuery>) -> (StatusCode, String) {
    let Some(store) = &state.store else { return (StatusCode::SERVICE_UNAVAILABLE, "ObjectStore not configured".into()); };
    let ns = q.ns.unwrap_or_else(|| "default".into());
    let path = q.path.unwrap_or_else(|| q.key.clone());
    let hash = q.hash.unwrap_or_else(|| "sha256-dev".into());
    let page_id = q.page_id.unwrap_or(0);

    let ck = seekfs_core::types::CacheKey { ns: seekfs_core::types::Namespace(ns), item: seekfs_core::types::PathAndVersion { path, file_hash: hash }, page_id };
    let mut sink = match state.l2.put_stream(ck.clone(), Some(q.len as usize)) { Ok(s) => s, Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 put_stream error: {e}")) };

    let key_buf = file_key(&ck.ns.0, &ck.item.path, Some(&ck.item.file_hash));
    let mut stream = match store.get_range_stream(&key_buf, q.off..(q.off+q.len)).await {
        Ok(s) => s,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("store stream error: {e}")),
    };

    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(b) => { total += b.len(); if let Err(e) = sink.write(&b) { return (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 write error: {e}")); } },
            Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, format!("store stream chunk error: {e}")),
        }
    }
    match sink.commit() {
        Ok(outcome) => (StatusCode::OK, format!("streamed {total} bytes to L2 ({:?})", outcome)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 commit error: {e}")),
    }
}

#[derive(Debug, serde::Deserialize)]
struct BlobQuery {
    ns: String,
    path: String,
    off: u64,
    len: u64,
    hash: Option<String>,
}

async fn get_blob(State(state): State<Arc<AppState>>, Query(q): Query<BlobQuery>) -> Result<(StatusCode, axum::http::HeaderMap, Bytes), (StatusCode, String)> {
    let store = state.store.clone().ok_or((StatusCode::SERVICE_UNAVAILABLE, "ObjectStore not configured".to_string()))?;
    // Resolve manifest and file entry
    let manif = state.manifests.get_or_load(store.clone(), &q.ns).await.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("manifest load: {e}")))?;
    let fe = find_file(&manif.manifest, &q.path).ok_or((StatusCode::NOT_FOUND, "file not in manifest".to_string()))?;
    let hash = q.hash.as_deref().unwrap_or(&fe.hash);
    let file_size = fe.size;
    let storage_key = fe.storage.key.clone();
    let end = q.off.saturating_add(q.len).min(file_size);
    if q.off >= file_size { return Err((StatusCode::RANGE_NOT_SATISFIABLE, "offset beyond EOF".into())); }
    if end <= q.off { return Err((StatusCode::BAD_REQUEST, "len must be > 0".into())); }

    // Compute slab range
    let slab = state.slab_size;
    let first = (q.off / slab) as u32;
    let last = ((end - 1) / slab) as u32;

    // Ensure slabs in L2
    for pid in first..=last {
        let ck = seekfs_core::types::CacheKey {
            ns: seekfs_core::types::Namespace(q.ns.clone()),
            item: seekfs_core::types::PathAndVersion { path: q.path.clone(), file_hash: hash.to_string() },
            page_id: pid,
        };
        if state.l2.get(&ck).is_none() {
            // fetch page range
            let start = (pid as u64) * slab;
            let mut len = slab;
            if start + len > file_size { len = file_size - start; }
            let mut stream = store.get_range_stream(&storage_key, start..start+len)
                .await
                .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("store stream error: {e}")))?;
            let mut page_buf: Vec<u8> = Vec::with_capacity(len as usize);
            while let Some(chunk) = stream.next().await {
                let b = chunk.map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("store chunk: {e}")))?;
                page_buf.extend_from_slice(&b);
            }
            if let Some(exp) = page_crc_by_id(fe, pid) {
                let got = crc32c::crc32c(&page_buf);
                if got != exp {
                    return Err((StatusCode::SERVICE_UNAVAILABLE, format!("crc32c mismatch for page {}: got {}, expected {}", pid, got, exp)));
                }
            }
            let mut sink = state.l2.put_stream(ck, Some(page_buf.len()))
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 put_stream error: {e}")))?;
            sink.write(&page_buf).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 write: {e}")))?;
            let _ = sink.commit().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("l2 commit: {e}")))?;
        }
    }

    // Stitch response
    let mut out = vec![0u8; (end - q.off) as usize];
    let mut copied = 0usize;
    let mut cur_off = q.off;
    for pid in first..=last {
        let ck = seekfs_core::types::CacheKey {
            ns: seekfs_core::types::Namespace(q.ns.clone()),
            item: seekfs_core::types::PathAndVersion { path: q.path.clone(), file_hash: hash.to_string() },
            page_id: pid,
        };
        let slab_bytes = state.l2.get(&ck).ok_or((StatusCode::INTERNAL_SERVER_ERROR, "l2 missing after fetch".to_string()))?;
        let slab_start = (pid as u64) * slab;
        let from = if cur_off > slab_start { (cur_off - slab_start) as usize } else { 0 };
        let to = ((end - slab_start) as usize).min(slab_bytes.len());
        let chunk = &slab_bytes[from..to];
        out[copied..(copied + chunk.len())].copy_from_slice(chunk);
        copied += chunk.len();
        cur_off = slab_start + to as u64;
    }

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from_str(&out.len().to_string()).unwrap());
    Ok((StatusCode::OK, headers, Bytes::from(out)))
}
