use crate::domain::memory::{Memory, MemoryMetadata, GraphStatus};
use uuid::Uuid;

#[derive(Clone)]
pub struct MemoryState;

impl MemoryState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MemoryState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn search_memories(_query: String) -> Result<Vec<Memory>, String> {
    Ok(vec![
        Memory {
            id: Uuid::new_v4().to_string(),
            content: "User prefers dark mode with yellow accent".to_string(),
            created_at: chrono::Utc::now() - chrono::Duration::hours(2),
            metadata: MemoryMetadata {
                category: Some("Preferences".to_string()),
                tags: vec!["ui".to_string(), "theme".to_string()],
                importance: Some(0.8),
            },
        },
        Memory {
            id: Uuid::new_v4().to_string(),
            content: "Project uses Tauri v2 with Leptos 0.8".to_string(),
            created_at: chrono::Utc::now() - chrono::Duration::days(1),
            metadata: MemoryMetadata {
                category: Some("Technical".to_string()),
                tags: vec!["tech-stack".to_string()],
                importance: Some(0.9),
            },
        },
    ])
}

#[tauri::command]
pub async fn get_graph_status() -> Result<GraphStatus, String> {
    Ok(GraphStatus {
        total_memories: 42,
        last_updated: chrono::Utc::now() - chrono::Duration::hours(2),
        storage_size: 2_400_000,
    })
}

#[tauri::command]
pub async fn store_memory(content: String, metadata: MemoryMetadata) -> Result<Memory, String> {
    Ok(Memory {
        id: Uuid::new_v4().to_string(),
        content,
        created_at: chrono::Utc::now(),
        metadata,
    })
}

#[tauri::command]
pub async fn delete_memory(_memory_id: String) -> Result<(), String> {
    Ok(())
}
