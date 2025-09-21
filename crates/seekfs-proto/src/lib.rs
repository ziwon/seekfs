use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadvItem {
    pub path: String,
    pub off: u64,
    pub len: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HintsWindow {
    pub path: String,
    pub ranges: Vec<(u64, u32)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HintsRequest {
    pub epoch: u64,
    #[serde(default)]
    pub priority: Option<String>, // normal|high
    pub windows: Vec<HintsWindow>,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BarrierResponse {
    pub barrier_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorItem {
    pub path: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorResponse {
    pub top_warm_candidates: Vec<AdvisorItem>,
}

