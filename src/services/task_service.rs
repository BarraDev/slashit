use crate::models::*;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

pub struct CreateTaskParams {
    pub project_id: String,
    pub title: String,
    pub description: Option<String>,
    pub model: String,
    pub planning_mode: bool,
    pub dependencies: Vec<String>,
    pub category: Option<TaskCategory>,
    pub priority: Option<TaskPriority>,
    pub complexity: Option<TaskComplexity>,
    pub impact: Option<TaskImpact>,
    pub security_severity: Option<SecuritySeverity>,
}

pub async fn create_task(params: CreateTaskParams) -> Result<Task, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "params": {
            "projectId": params.project_id,
            "title": params.title,
            "description": params.description,
            "model": params.model,
            "planningMode": params.planning_mode,
            "dependencies": params.dependencies,
            "category": params.category,
            "priority": params.priority,
            "complexity": params.complexity,
            "impact": params.impact,
            "securitySeverity": params.security_severity,
        }
    })).unwrap();

    let response = invoke("create_task", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn list_tasks(project_id: String) -> Result<Vec<Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "projectId": project_id })).unwrap();
    let response = invoke("list_tasks", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn update_task_status(task_id: String, status: TaskStatus) -> Result<Option<Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "status": status,
    })).unwrap();

    let response = invoke("update_task_status", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn toggle_subtask(task_id: String, subtask_id: String) -> Result<Option<Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "subtaskId": subtask_id,
    })).unwrap();

    let response = invoke("toggle_subtask", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub struct UpdateTaskParams {
    pub task_id: String,
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub category: Option<TaskCategory>,
    pub priority: Option<TaskPriority>,
    pub complexity: Option<TaskComplexity>,
    pub impact: Option<TaskImpact>,
    pub security_severity: Option<SecuritySeverity>,
    pub model: Option<String>,
    pub planning_mode: Option<bool>,
}

pub async fn update_task(params: UpdateTaskParams) -> Result<Option<Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "params": {
            "taskId": params.task_id,
            "title": params.title,
            "description": params.description,
            "category": params.category,
            "priority": params.priority,
            "complexity": params.complexity,
            "impact": params.impact,
            "securitySeverity": params.security_severity,
            "model": params.model,
            "planningMode": params.planning_mode,
        }
    })).unwrap();

    let response = invoke("update_task", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn delete_task(task_id: String) -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
    })).unwrap();

    let response = invoke("delete_task", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn reorder_task(
    task_id: String,
    new_status: Option<TaskStatus>,
    new_position: i32,
) -> Result<Option<Task>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "taskId": task_id,
        "newStatus": new_status,
        "newPosition": new_position,
    })).unwrap();

    let response = invoke("reorder_task", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}
