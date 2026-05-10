use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum RoadmapStatus {
    #[default]
    Proposed,
    Planned,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum TargetAudience {
    #[default]
    Technical,
    Business,
    EndUser,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MarketOpportunity {
    High,
    #[default]
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoadmapFeature {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: String,
    pub description: String,
    pub motivation: Option<String>,
    pub status: RoadmapStatus,
    pub priority: crate::domain::TaskPriority,
    pub audience: TargetAudience,
    pub complexity: crate::domain::TaskComplexity,
    pub competitor_analysis: Option<CompetitorAnalysis>,
    pub linked_task_ids: Vec<Uuid>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompetitorAnalysis {
    pub competitors: Vec<CompetitorFeature>,
    pub gap_analysis: String,
    pub market_opportunity: MarketOpportunity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompetitorFeature {
    pub competitor_name: String,
    pub feature_name: String,
    pub url: Option<String>,
    pub has_feature: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFeatureRequest {
    pub project_id: Uuid,
    pub title: String,
    pub description: String,
    pub motivation: Option<String>,
    pub priority: crate::domain::TaskPriority,
    pub audience: TargetAudience,
    pub complexity: crate::domain::TaskComplexity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateFeatureRequest {
    pub title: Option<String>,
    pub description: Option<String>,
    pub motivation: Option<String>,
    pub status: Option<RoadmapStatus>,
    pub priority: Option<crate::domain::TaskPriority>,
    pub audience: Option<TargetAudience>,
    pub complexity: Option<crate::domain::TaskComplexity>,
    pub linked_task_ids: Option<Vec<Uuid>>,
}



