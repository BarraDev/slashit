use crate::domain::{
    RoadmapFeature, CreateFeatureRequest, UpdateFeatureRequest,
    RoadmapStatus,
};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct RoadmapState {
    pub features: Arc<RwLock<HashMap<Uuid, RoadmapFeature>>>,
}

impl RoadmapState {
    pub fn new() -> Self {
        Self {
            features: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for RoadmapState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn create_roadmap_feature(
    state: tauri::State<'_, crate::AppState>,
    request: CreateFeatureRequest,
) -> Result<RoadmapFeature, String> {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();

    let feature = RoadmapFeature {
        id,
        project_id: request.project_id,
        title: request.title,
        description: request.description,
        motivation: request.motivation,
        status: RoadmapStatus::default(),
        priority: request.priority,
        audience: request.audience,
        complexity: request.complexity,
        competitor_analysis: None,
        linked_task_ids: Vec::new(),
        created_at: now,
        updated_at: now,
    };

    let mut features = state.roadmap.features.write().await;
    features.insert(id, feature.clone());

    Ok(feature)
}

#[tauri::command]
pub async fn update_roadmap_feature(
    state: tauri::State<'_, crate::AppState>,
    feature_id: String,
    request: UpdateFeatureRequest,
) -> Result<Option<RoadmapFeature>, String> {
    let feature_id = Uuid::parse_str(&feature_id).map_err(|e| e.to_string())?;
    let mut features = state.roadmap.features.write().await;

    if let Some(feature) = features.get_mut(&feature_id) {
        if let Some(title) = request.title {
            feature.title = title;
        }
        if let Some(description) = request.description {
            feature.description = description;
        }
        if let Some(motivation) = request.motivation {
            feature.motivation = Some(motivation);
        }
        if let Some(status) = request.status {
            feature.status = status;
        }
        if let Some(priority) = request.priority {
            feature.priority = priority;
        }
        if let Some(audience) = request.audience {
            feature.audience = audience;
        }
        if let Some(complexity) = request.complexity {
            feature.complexity = complexity;
        }
        if let Some(linked_task_ids) = request.linked_task_ids {
            feature.linked_task_ids = linked_task_ids;
        }
        feature.updated_at = chrono::Utc::now();

        Ok(Some(feature.clone()))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn delete_roadmap_feature(
    state: tauri::State<'_, crate::AppState>,
    feature_id: String,
) -> Result<bool, String> {
    let feature_id = Uuid::parse_str(&feature_id).map_err(|e| e.to_string())?;
    let mut features = state.roadmap.features.write().await;

    Ok(features.remove(&feature_id).is_some())
}

#[tauri::command]
pub async fn list_roadmap_features(
    state: tauri::State<'_, crate::AppState>,
    project_id: Option<String>,
    status: Option<String>,
) -> Result<Vec<RoadmapFeature>, String> {
    let features = state.roadmap.features.read().await;
    let mut result: Vec<RoadmapFeature> = features.values().cloned().collect();

    if let Some(project_id_str) = project_id {
        let project_id = Uuid::parse_str(&project_id_str).map_err(|e| e.to_string())?;
        result.retain(|f| f.project_id == project_id);
    }

    if let Some(status_str) = status {
        let status_enum = match status_str.as_str() {
            "proposed" => Ok(RoadmapStatus::Proposed),
            "planned" => Ok(RoadmapStatus::Planned),
            "in_progress" => Ok(RoadmapStatus::InProgress),
            "completed" => Ok(RoadmapStatus::Completed),
            "cancelled" => Ok(RoadmapStatus::Cancelled),
            _ => Err(format!("Invalid status: {}", status_str)),
        };
        if let Ok(s) = status_enum {
            result.retain(|f| f.status == s);
        }
    }

    fn priority_value(p: &crate::domain::TaskPriority) -> u8 {
        match p {
            crate::domain::TaskPriority::Urgent => 4,
            crate::domain::TaskPriority::High => 3,
            crate::domain::TaskPriority::Medium => 2,
            crate::domain::TaskPriority::Low => 1,
        }
    }

    result.sort_by(|a, b| {
        priority_value(&b.priority)
            .cmp(&priority_value(&a.priority))
            .then_with(|| b.created_at.cmp(&a.created_at))
    });

    Ok(result)
}

#[tauri::command]
pub async fn get_roadmap_feature(
    state: tauri::State<'_, crate::AppState>,
    feature_id: String,
) -> Result<Option<RoadmapFeature>, String> {
    let feature_id = Uuid::parse_str(&feature_id).map_err(|e| e.to_string())?;
    let features = state.roadmap.features.read().await;

    Ok(features.get(&feature_id).cloned())
}

#[tauri::command]
pub async fn link_task_to_feature(
    state: tauri::State<'_, crate::AppState>,
    feature_id: String,
    task_id: String,
) -> Result<Option<RoadmapFeature>, String> {
    let feature_id = Uuid::parse_str(&feature_id).map_err(|e| e.to_string())?;
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let mut features = state.roadmap.features.write().await;

    if let Some(feature) = features.get_mut(&feature_id) {
        if !feature.linked_task_ids.contains(&task_id) {
            feature.linked_task_ids.push(task_id);
        }
        feature.updated_at = chrono::Utc::now();
        Ok(Some(feature.clone()))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn unlink_task_from_feature(
    state: tauri::State<'_, crate::AppState>,
    feature_id: String,
    task_id: String,
) -> Result<Option<RoadmapFeature>, String> {
    let feature_id = Uuid::parse_str(&feature_id).map_err(|e| e.to_string())?;
    let task_id = Uuid::parse_str(&task_id).map_err(|e| e.to_string())?;

    let mut features = state.roadmap.features.write().await;

    if let Some(feature) = features.get_mut(&feature_id) {
        feature.linked_task_ids.retain(|id| id != &task_id);
        feature.updated_at = chrono::Utc::now();
        Ok(Some(feature.clone()))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_ask_for_the_state_the_app_actually_manages() {
        // `run()` calls `.manage()` exactly once, with `AppState`. Tauri looks
        // `State<T>` up by `TypeId` in that map, so a command asking for
        // `State<RoadmapState>` — a type that only ever exists as a *field* of
        // `AppState` — is rejected at invoke time with "state not managed for
        // field ... You must call `.manage()` before using this command". Every
        // command below is registered and reachable from the roadmap page, so
        // the whole page failed.
        //
        // The helper is deliberately never awaited: `tauri::State` wraps a
        // private field and cannot be constructed outside a running app, so the
        // type check *is* the assertion. Reverting any signature to the
        // sub-state stops this file compiling.
        async fn binds_to_app_state(
            state: tauri::State<'_, crate::AppState>,
            create: CreateFeatureRequest,
            update: UpdateFeatureRequest,
            id: String,
        ) {
            let _ = create_roadmap_feature(state.clone(), create).await;
            let _ = update_roadmap_feature(state.clone(), id.clone(), update).await;
            let _ = delete_roadmap_feature(state.clone(), id.clone()).await;
            let _ = list_roadmap_features(state.clone(), None, None).await;
            let _ = get_roadmap_feature(state.clone(), id.clone()).await;
            let _ = link_task_to_feature(state.clone(), id.clone(), id.clone()).await;
            let _ = unlink_task_from_feature(state, id.clone(), id).await;
        }

        let _ = &binds_to_app_state;
    }
}
