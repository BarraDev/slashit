use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoadmapStatus {
    Proposed,
    Planned,
    InProgress,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TargetAudience {
    Technical,
    Business,
    EndUser,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketOpportunity {
    High,
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
    pub priority: super::TaskPriority,
    pub audience: TargetAudience,
    pub complexity: super::TaskComplexity,
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
pub struct CompetitorAnalysisRequest {
    pub feature_id: Uuid,
    pub competitors: Vec<String>,
}
