#[tauri::command]
pub async fn force_quit(app: tauri::AppHandle) {
    app.exit(0);
}

#[tauri::command]
pub async fn get_active_process_count(
    state: tauri::State<'_, crate::AppState>,
) -> Result<serde_json::Value, String> {
    let pty_count = state.pty.sessions.lock().await.len();
    let executor = state.executor.get().map(|executor| executor.as_ref());
    let agent_count = crate::commands::agent::active_agent_count(&state.agent.executions, executor).await;
    Ok(serde_json::json!({
        "pty": pty_count,
        "agents": agent_count,
    }))
}
