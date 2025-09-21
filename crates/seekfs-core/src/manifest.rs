use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PageInfo {
    pub page_id: u32,
    pub off: u64,
    pub len: u32,
    pub crc32c: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectLoc {
    pub key: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub sse: Option<String>, // kms|none
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub hash: String, // sha256 content hash (versioning)
    pub page_table: Vec<PageInfo>,
    pub storage: ObjectLoc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: u64,
    pub files: Vec<FileEntry>,
    #[serde(default)]
    pub parents: Vec<u64>,
    #[serde(default)]
    pub tombstones: Vec<String>,
    pub created_at: u64,
}

