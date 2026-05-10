use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryMetadata {
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub importance: Option<f32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub metadata: MemoryMetadata,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphStatus {
    pub total_memories: usize,
    pub last_updated: DateTime<Utc>,
    pub storage_size: u64,
}
