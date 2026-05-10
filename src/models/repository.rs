use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: Uuid,
    pub local_path: String,
    pub remote_url: Option<String>,
    pub remote_type: Option<RemoteType>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteType {
    GitHub,
    GitLab,
    Bitbucket,
    Other(String),
}
