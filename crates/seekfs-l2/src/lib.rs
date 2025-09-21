use parking_lot::RwLock;
use seekfs_core::{l2::{L2Backend, L2Error, PutOutcome, StatsSnapshot, L2Put}, types::CacheKey};
use std::{collections::HashMap, sync::Arc};

#[cfg(feature = "mem")]
#[derive(Clone, Default)]
pub struct MemL2 {
    inner: Arc<RwLock<Inner>>,
}

#[derive(Default)]
struct Inner {
    map: HashMap<CacheKey, Vec<u8>>,
    bytes: u64,
}

#[cfg(feature = "mem")]
impl MemL2 {
    pub fn new() -> Self { Self::default() }
}

#[cfg(feature = "mem")]
impl L2Backend for MemL2 {
    fn name(&self) -> &str { "mem" }

    fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        self.inner.read().map.get(key).cloned()
    }

    fn put(&self, key: CacheKey, value: Vec<u8>) -> Result<PutOutcome, L2Error> {
        let mut g = self.inner.write();
        let prev = g.map.insert(key, value.clone());
        if let Some(ref p) = prev { g.bytes -= p.len() as u64; }
        g.bytes += value.len() as u64;
        Ok(match prev { Some(_) => PutOutcome::Updated, None => PutOutcome::Inserted })
    }

    fn delete(&self, key: &CacheKey) -> Result<(), L2Error> {
        let mut g = self.inner.write();
        if let Some(v) = g.map.remove(key) { g.bytes -= v.len() as u64; }
        Ok(())
    }

    fn stats(&self) -> StatsSnapshot {
        let g = self.inner.read();
        StatsSnapshot { items: g.map.len() as u64, bytes: g.bytes }
    }

    fn put_stream(&self, key: CacheKey, expected_len: Option<usize>) -> Result<Box<dyn L2Put>, L2Error> {
        Ok(Box::new(MemPut { inner: self.inner.clone(), key, buf: Vec::with_capacity(expected_len.unwrap_or(0)) }))
    }
}

// Placeholders to keep feature flags visible without implementation yet
#[cfg(feature = "blobdb")]
pub struct BlobDbL2;
#[cfg(feature = "slabfile")]
#[derive(Clone)]
pub struct SlabfileL2 { inner: Arc<SlabfileInner> }

#[cfg(feature = "slabfile")]
struct SlabfileInner {
    base: std::path::PathBuf,
    stats: Arc<parking_lot::RwLock<(u64, u64)>>,
    index: Arc<redb::Database>,
    active: parking_lot::Mutex<ActiveSegment>,
}

#[cfg(feature = "slabfile")]
struct ActiveSegment { id: u32, file: std::fs::File, size: u64 }

impl SlabfileL2 {
    pub fn new<P: Into<std::path::PathBuf>>(base: P) -> std::io::Result<Self> {
        let base = base.into();
        std::fs::create_dir_all(&base)?;
        let db_path = base.join("index.redb");
        let index = Arc::new(redb::Database::create(db_path).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?);
        // discover or create active segment
        let (seg_id, size) = discover_last_segment(&base)?;
        let seg_file = open_segment(&base, seg_id)?;
        let (items, bytes) = scan_dir(&base);
        let inner = SlabfileInner {
            base,
            stats: Arc::new(parking_lot::RwLock::new((items, bytes))),
            index,
            active: parking_lot::Mutex::new(ActiveSegment { id: seg_id, file: seg_file, size }),
        };
        Ok(Self { inner: Arc::new(inner) })
    }
}

#[cfg(feature = "slabfile")]
fn scan_dir(base: &std::path::Path) -> (u64, u64) {
    let mut items = 0u64;
    let mut bytes = 0u64;
    let mut stack = vec![base.to_path_buf()];
    while let Some(p) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                let path = e.path();
                if let Ok(ft) = e.file_type() {
                    if ft.is_dir() { stack.push(path); continue; }
                }
                if let Ok(meta) = e.metadata() { items += 1; bytes += meta.len(); }
            }
        }
    }
    (items, bytes)
}

#[cfg(feature = "slabfile")]
fn discover_last_segment(base: &std::path::Path) -> std::io::Result<(u32, u64)> {
    let mut max_id = 0u32;
    if let Ok(rd) = std::fs::read_dir(base) {
        for e in rd.flatten() {
            let name = e.file_name();
            if let Some(s) = name.to_str() {
                if let Some(t) = s.strip_prefix("data-") { if let Some(t2) = t.strip_suffix(".seg") {
                    if let Ok(id) = t2.parse::<u32>() { if id > max_id { max_id = id; } }
                }}
            }
        }
    }
    if max_id == 0 { max_id = 1; }
    let path = base.join(format!("data-{max_id:05}.seg"));
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok((max_id, size))
}

#[cfg(feature = "slabfile")]
fn open_segment(base: &std::path::Path, id: u32) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let path = base.join(format!("data-{id:05}.seg"));
    OpenOptions::new().create(true).append(true).read(true).open(path)
}

#[cfg(feature = "slabfile")]
fn key_digest(key: &CacheKey) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(key.ns.0.as_bytes());
    hasher.update(b"|");
    hasher.update(key.item.path.as_bytes());
    hasher.update(b"|");
    hasher.update(key.item.file_hash.as_bytes());
    hasher.update(b"|");
    hasher.update(key.page_id.to_le_bytes());
    let out = hasher.finalize();
    hex::encode(out)
}

#[cfg(feature = "slabfile")]
fn digest_to_path(base: &std::path::Path, hex: &str) -> std::path::PathBuf {
    if hex.len() >= 4 {
        let (a, b) = hex.split_at(2);
        let (b1, rest) = b.split_at(2);
        base.join(a).join(b1).join(format!("{}.slab", rest))
    } else {
        base.join(format!("{}.slab", hex))
    }
}

#[cfg(feature = "slabfile")]
struct SlabPut {
    base: std::path::PathBuf,
    stats: Arc<parking_lot::RwLock<(u64,u64)>>,
    key: CacheKey,
    tmp: tempfile::NamedTempFile,
    bytes: usize,
}

#[cfg(feature = "slabfile")]
impl L2Put for SlabPut {
    fn write(&mut self, chunk: &[u8]) -> Result<(), L2Error> {
        use std::io::Write;
        self.tmp.write_all(chunk).map_err(|e| L2Error::Backend(e.to_string()))?;
        self.bytes += chunk.len();
        Ok(())
    }

    fn commit(mut self: Box<Self>) -> Result<PutOutcome, L2Error> {
        let hex = key_digest(&self.key);
        let target = digest_to_path(&self.base, &hex);
        if let Some(dir) = target.parent() { std::fs::create_dir_all(dir).map_err(|e| L2Error::Backend(e.to_string()))?; }
        let prev_len = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        self.tmp.persist(&target).map_err(|e| L2Error::Backend(e.error.to_string()))?;
        let mut g = self.stats.write();
        if prev_len == 0 { g.0 += 1; }
        g.1 = g.1 + (self.bytes as u64) - prev_len;
        Ok(if prev_len == 0 { PutOutcome::Inserted } else { PutOutcome::Updated })
    }
}

#[cfg(feature = "slabfile")]
impl L2Backend for SlabfileL2 {
    fn name(&self) -> &str { "slabfile" }

    fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        // First try index
        if let Some((seg, off, len)) = self.inner.index_lookup(key) {
            let path = self.inner.base.join(format!("data-{seg:05}.seg"));
            let mut f = std::fs::File::open(path).ok()?;
            use std::io::{Seek, SeekFrom, Read};
            f.seek(SeekFrom::Start(off)).ok()?;
            let mut buf = vec![0u8; len as usize];
            if f.read_exact(&mut buf).is_ok() { return Some(buf); }
        }
        // Backward-compat: hashed page file
        let hex = key_digest(key);
        let path = digest_to_path(&self.inner.base, &hex);
        std::fs::read(path).ok()
    }

    fn put(&self, key: CacheKey, value: Vec<u8>) -> Result<PutOutcome, L2Error> {
        // Append to active segment
        let (seg_id, off) = self.inner.append_to_active(&value).map_err(|e| L2Error::Backend(e.to_string()))?;
        let existed = self.inner.index_put(&key, seg_id, off, value.len() as u32).map_err(|e| L2Error::Backend(e.to_string()))?;
        let mut g = self.inner.stats.write();
        if !existed { g.0 += 1; }
        g.1 += value.len() as u64;
        Ok(if existed { PutOutcome::Updated } else { PutOutcome::Inserted })
    }

    fn delete(&self, key: &CacheKey) -> Result<(), L2Error> {
        // Remove from index (do not reclaim segment space)
        let removed = self.inner.index_delete(key).map_err(|e| L2Error::Backend(e.to_string()))?;
        if removed {
            let mut g = self.inner.stats.write();
            g.0 = g.0.saturating_sub(1);
            // bytes unchanged (space not reclaimed)
        }
        Ok(())
    }

    fn stats(&self) -> StatsSnapshot {
        let g = self.inner.stats.read();
        StatsSnapshot { items: g.0, bytes: g.1 }
    }

    fn put_stream(&self, key: CacheKey, _expected_len: Option<usize>) -> Result<Box<dyn L2Put>, L2Error> {
        // Collect in memory; commit appends to active segment and updates index
        struct SegPut { key: CacheKey, buf: Vec<u8>, inner: Arc<SlabfileInner> }
        impl L2Put for SegPut {
            fn write(&mut self, chunk: &[u8]) -> Result<(), L2Error> { self.buf.extend_from_slice(chunk); Ok(()) }
            fn commit(self: Box<Self>) -> Result<PutOutcome, L2Error> {
                let (seg_id, off) = self.inner.append_to_active(&self.buf).map_err(|e| L2Error::Backend(e.to_string()))?;
                let existed = self.inner.index_put(&self.key, seg_id, off, self.buf.len() as u32).map_err(|e| L2Error::Backend(e.to_string()))?;
                let mut g = self.inner.stats.write();
                if !existed { g.0 += 1; }
                g.1 += self.buf.len() as u64;
                Ok(if existed { PutOutcome::Updated } else { PutOutcome::Inserted })
            }
        }
        Ok(Box::new(SegPut { key, buf: Vec::new(), inner: self.inner.clone() }))
    }
}

#[cfg(feature = "slabfile")]
impl SlabfileInner {
    fn append_to_active(&self, data: &[u8]) -> std::io::Result<(u32, u64)> {
        const MAX_SEG_BYTES: u64 = 1_073_741_824; // 1 GiB
        use std::io::{Seek, SeekFrom, Write};
        let mut guard = self.active.lock();
        if guard.size + data.len() as u64 > MAX_SEG_BYTES {
            // roll segment
            let new_id = guard.id + 1;
            guard.file = open_segment(&self.base, new_id)?;
            guard.id = new_id;
            guard.size = 0;
        }
        let off = guard.size;
        guard.file.seek(SeekFrom::Start(off))?; // ensure position
        guard.file.write_all(data)?;
        guard.size += data.len() as u64;
        Ok((guard.id, off))
    }

    fn index_put(&self, key: &CacheKey, seg_id: u32, off: u64, len: u32) -> std::io::Result<bool> {
        use redb::{TableDefinition, ReadableTable};
        const T: TableDefinition<&[u8], &[u8]> = TableDefinition::new("idx");
        let digest = key_digest(key);
        let db = &self.index;
        let tx = db.begin_write().map_err(to_ioe)?;
        let existed;
        {
            let mut table = tx.open_table(T).map_err(to_ioe)?;
            existed = table.get(digest.as_bytes()).map_err(to_ioe)?.is_some();
            let mut val = [0u8; 16];
            val[0..4].copy_from_slice(&seg_id.to_le_bytes());
            val[4..12].copy_from_slice(&off.to_le_bytes());
            val[12..16].copy_from_slice(&len.to_le_bytes());
            table.insert(digest.as_bytes(), &val[..]).map_err(to_ioe)?;
        }
        tx.commit().map_err(to_ioe)?;
        Ok(existed)
    }

    fn index_lookup(&self, key: &CacheKey) -> Option<(u32, u64, u32)> {
        use redb::TableDefinition;
        const T: TableDefinition<&[u8], &[u8]> = TableDefinition::new("idx");
        let digest = key_digest(key);
        let tx = self.index.begin_read().ok()?;
        let table = tx.open_table(T).ok()?;
        let v = table.get(digest.as_bytes()).ok()??;
        let val = v.value();
        if val.len() != 16 { return None; }
        let mut seg_id_bytes = [0u8;4]; seg_id_bytes.copy_from_slice(&val[0..4]);
        let mut off_bytes = [0u8;8]; off_bytes.copy_from_slice(&val[4..12]);
        let mut len_bytes = [0u8;4]; len_bytes.copy_from_slice(&val[12..16]);
        let seg_id = u32::from_le_bytes(seg_id_bytes);
        let off = u64::from_le_bytes(off_bytes);
        let len = u32::from_le_bytes(len_bytes);
        Some((seg_id, off, len))
    }

    fn index_delete(&self, key: &CacheKey) -> std::io::Result<bool> {
        use redb::{TableDefinition, ReadableTable};
        const T: TableDefinition<&[u8], &[u8]> = TableDefinition::new("idx");
        let digest = key_digest(key);
        let tx = self.index.begin_write().map_err(to_ioe)?;
        let existed;
        {
            let mut table = tx.open_table(T).map_err(to_ioe)?;
            existed = table.remove(digest.as_bytes()).map_err(to_ioe)?.is_some();
        }
        tx.commit().map_err(to_ioe)?;
        Ok(existed)
    }
}

#[cfg(feature = "slabfile")]
fn to_ioe<E: std::fmt::Display>(e: E) -> std::io::Error { std::io::Error::new(std::io::ErrorKind::Other, e.to_string()) }

struct MemPut {
    inner: Arc<RwLock<Inner>>,
    key: CacheKey,
    buf: Vec<u8>,
}

impl L2Put for MemPut {
    fn write(&mut self, chunk: &[u8]) -> Result<(), L2Error> {
        self.buf.extend_from_slice(chunk);
        Ok(())
    }

    fn commit(self: Box<Self>) -> Result<PutOutcome, L2Error> {
        let mut g = self.inner.write();
        let prev = g.map.insert(self.key, self.buf);
        if let Some(ref p) = prev { g.bytes -= p.len() as u64; }
        let new_bytes = g.map.values().map(|v| v.len() as u64).sum();
        g.bytes = new_bytes;
        Ok(match prev { Some(_) => PutOutcome::Updated, None => PutOutcome::Inserted })
    }
}
