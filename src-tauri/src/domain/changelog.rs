use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ChangelogSource {
    CompletedTasks,
    GitHistory,
    BranchComparison,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChangelogOptions {
    pub count: Option<usize>,
    pub include_merges: bool,
    pub base_branch: Option<String>,
    pub compare_branch: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitCommit {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub date: DateTime<Utc>,
    pub merges: Option<Vec<String>>,
}
