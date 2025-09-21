use std::{collections::HashMap, io::Read, sync::Arc, time::{Duration, Instant}};

use flate2::read::GzDecoder;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use seekfs_core::{layout::{manifest_head_key, manifest_version_key}, Manifest, FileEntry};
use seekfs_core::store::{ObjectStore, StoreError};

#[derive(Clone)]
pub struct ManifestCache {
    inner: Arc<RwLock<HashMap<String, ManifestState>>>,
    ttl: Duration,
}

#[derive(Clone)]
pub struct ManifestState {
    pub version: u64,
    pub manifest: Arc<Manifest>,
    pub loaded_at: Instant,
}

impl ManifestCache {
    pub fn new(ttl: Duration) -> Self {
        Self { inner: Arc::new(RwLock::new(HashMap::new())), ttl }
    }

    pub async fn get_or_load(&self, store: Arc<dyn ObjectStore>, ns: &str) -> Result<ManifestState, StoreError> {
        if let Some(state) = self.try_get_fresh(ns).await { return Ok(state); }
        self.reload(store, ns).await
    }

    async fn try_get_fresh(&self, ns: &str) -> Option<ManifestState> {
        let g = self.inner.read().await;
        let st = g.get(ns)?;
        if st.loaded_at.elapsed() < self.ttl { Some(st.clone()) } else { None }
    }

    async fn reload(&self, store: Arc<dyn ObjectStore>, ns: &str) -> Result<ManifestState, StoreError> {
        let head_key = manifest_head_key(ns);
        // HEAD file is tiny; fetch first 1 KiB
        let bytes = store.get_range(&head_key, 0..1024).await?;
        let s = String::from_utf8_lossy(&bytes);
        let v = parse_head_version(&s).ok_or_else(|| StoreError::Head(format!("invalid HEAD content: {}", s)))?;
        let man_key = manifest_version_key(ns, v);
        let size = store.head(&man_key).await?.and_then(|h| h.size).ok_or_else(|| StoreError::Head("manifest size unknown".into()))?;
        let gz = store.get_range(&man_key, 0..size).await?;
        let mut dec = GzDecoder::new(std::io::Cursor::new(gz));
        let mut out = Vec::new();
        dec.read_to_end(&mut out).map_err(|e| StoreError::Client(format!("gzip decode: {e}")))?;
        let manifest: Manifest = serde_json::from_slice(&out).map_err(|e| StoreError::Client(format!("json: {e}")))?;
        if manifest.version != v { warn!(ns, got = manifest.version, expected = v, "manifest version mismatch"); }
        let state = ManifestState { version: v, manifest: Arc::new(manifest), loaded_at: Instant::now() };
        let mut w = self.inner.write().await;
        w.insert(ns.to_string(), state.clone());
        info!(ns, version = v, "manifest loaded");
        Ok(state)
    }
}

fn parse_head_version(s: &str) -> Option<u64> {
    let line = s.lines().next()?.trim();
    if let Some(t) = line.strip_prefix("v-") { return t.parse::<u64>().ok(); }
    if let Some(t) = line.strip_prefix('v') { return t.parse::<u64>().ok(); }
    line.parse::<u64>().ok()
}

pub fn find_file<'a>(m: &'a Manifest, path: &str) -> Option<&'a FileEntry> {
    m.files.iter().find(|f| f.path == path)
}

pub fn page_crc_by_id<'a>(fe: &'a FileEntry, page_id: u32) -> Option<u32> {
    fe.page_table.iter().find(|p| p.page_id == page_id).map(|p| p.crc32c)
}
