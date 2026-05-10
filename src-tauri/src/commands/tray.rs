#[tauri::command]
pub async fn force_quit(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub async fn get_active_process_count(
    state: tauri::State<'_, crate::AppState>,
) -> Result<serde_json::Value, String> {
    let pty_count = state.pty.sessions.lock().await.len();
    let agent_count = {
        let execs = state.agent.executions.read().await;
        execs
            .values()
            .filter(|e| {
                matches!(
                    e.status,
                    crate::domain::AgentStatus::Running | crate::domain::AgentStatus::Starting
                )
            })
            .count()
    };
    Ok(serde_json::json!({
        "pty": pty_count,
        "agents": agent_count,
    }))
}
