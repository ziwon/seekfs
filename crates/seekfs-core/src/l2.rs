use crate::types::CacheKey;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
pub struct StatsSnapshot {
    pub items: u64,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum L2Error {
    #[error("backend error: {0}")]
    Backend(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PutOutcome {
    Inserted,
    Updated,
}

pub trait L2Backend: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn get(&self, key: &CacheKey) -> Option<Vec<u8>>;
    fn put(&self, key: CacheKey, value: Vec<u8>) -> Result<PutOutcome, L2Error>;
    fn delete(&self, key: &CacheKey) -> Result<(), L2Error>;
    fn stats(&self) -> StatsSnapshot;
    fn put_stream(&self, key: CacheKey, expected_len: Option<usize>) -> Result<Box<dyn L2Put>, L2Error>;
}

impl Display for dyn L2Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

pub trait L2Put: Send {
    fn write(&mut self, chunk: &[u8]) -> Result<(), L2Error>;
    fn commit(self: Box<Self>) -> Result<PutOutcome, L2Error>;
    fn abort(self: Box<Self>) {}
}
