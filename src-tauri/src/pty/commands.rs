use super::manager::{spawn_pty_session, resize_pty_session};
use super::store::SessionMetadata;
use serde::{Deserialize, Serialize};
use std::io::Read;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;
use chrono::Utc;

#[derive(Clone, Serialize, Deserialize)]
pub struct PtyInfo {
    pub id: String,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    pub is_new: bool,
    #[serde(default)]
    pub project_id: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct PtyOutput {
    pub session_id: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Serialize)]
pub struct PtyExit {
    pub session_id: String,
    pub exit_code: Option<i32>,
    pub reason: String,
}

/// Spawn a new PTY session with a shell
#[tauri::command]
pub async fn spawn_pty(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    working_directory: Option<String>,
    project_id: Option<String>,
) -> Result<PtyInfo, String> {
    let cols = cols.unwrap_or(80);
    let rows = rows.unwrap_or(24);
    let name = name.unwrap_or_else(|| "Terminal".to_string());

    println!("[PTY Backend] spawn_pty: name={}, size={}x{}, wd={:?}", name, cols, rows, working_directory);

    // Spawn the PTY session
    let (session, mut reader) = spawn_pty_session(name.clone(), cols, rows, working_directory.clone())
        .map_err(|e| {
            eprintln!("[PTY Backend] Failed to spawn PTY: {}", e);
            e
        })?;

    let session_id = session.id;
    
    let info = PtyInfo {
        id: session_id.to_string(),
        name: session.name.clone(),
        cols: session.cols,
        rows: session.rows,
        is_new: true,
        project_id: project_id.clone(),
    };
    
    // Create scrollback buffer for this session
    state.pty.scrollback.create(session_id).await;
    
    // Save session metadata
    let metadata = SessionMetadata {
        id: session_id,
        name: session.name.clone(),
        cols: session.cols,
        rows: session.rows,
        working_directory,
        created_at: Utc::now(),
        last_attached_at: Utc::now(),
        is_alive: true,
    };
    if let Err(e) = state.pty.store.upsert_session(metadata) {
        eprintln!("[PTY Backend] Failed to save session metadata: {}", e);
    }

    // Store the session
    {
        let mut sessions = state.pty.sessions.lock().await;
        sessions.insert(session_id, session);
    }

    // Spawn a background thread to read PTY output and emit events
    let app_handle = app.clone();
    let session_id_str = session_id.to_string();
    let scrollback = state.pty.scrollback.clone();
    let session_uuid = session_id;
    
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];

        let mut consecutive_empty_reads = 0;
        
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    // On Windows, we might get spurious empty reads
                    consecutive_empty_reads += 1;
                    if consecutive_empty_reads > 10 {
                        break;
                    }
                    // Small sleep to avoid busy loop on empty reads
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Ok(n) => {
                    consecutive_empty_reads = 0; // Reset counter on successful read
                    
                    // Store in scrollback buffer using blocking lock
                    let data_copy = buf[..n].to_vec();
                    let scrollback_clone = scrollback.clone();
                    let sid_for_scrollback = session_uuid;
                    // Use std::thread to avoid blocking the reader
                    let _ = std::thread::spawn(move || {
                        // Create a new runtime for this thread to run async code
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        rt.block_on(async {
                            scrollback_clone.append(sid_for_scrollback, &data_copy).await;
                        });
                    });
                    
                    let output = PtyOutput {
                        session_id: session_id_str.clone(),
                        data: buf[..n].to_vec(),
                    };
                    
                    // Emit event to frontend
                    if let Err(e) = app_handle.emit("pty-output", output) {
                        eprintln!("[PTY Backend] Failed to emit event: {}", e);
                    }
                }
                Err(e) => {
                    // Check if it's just a timeout or EOF condition
                    let err_str = e.to_string();
                    if err_str.contains("timed out") || err_str.contains("would block") {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    eprintln!("[PTY Backend] Read ERROR for session {}: {}", session_id_str, e);
                    
                    // Emit exit event with error info
                    let exit_event = PtyExit {
                        session_id: session_id_str.clone(),
                        exit_code: Some(1), // Non-zero indicates error
                        reason: format!("Error: {}", err_str),
                    };
                    if let Err(emit_err) = app_handle.emit("pty-exit", exit_event) {
                        eprintln!("[PTY Backend] Failed to emit exit event: {}", emit_err);
                    }
                    return; // Exit the thread without emitting another event
                }
            }
        }

        
        // Add a small delay to ensure the last output is fully processed by the frontend
        // before we emit the exit event. This prevents the exit overlay from appearing
        // before the final output is rendered.
        std::thread::sleep(std::time::Duration::from_millis(200));
        
        // Determine exit code based on how we exited
        // If we got too many empty reads or a clean EOF, assume normal exit (code 0)
        // If we exited due to an error, report that
        let (exit_code, reason) = if consecutive_empty_reads > 10 {
            // Clean exit - shell closed normally
            (Some(0), "Shell exited normally".to_string())
        } else {
            // We broke out of the loop - could be error or EOF
            (Some(0), "Shell exited".to_string())
        };
        
        // Emit exit event to frontend
        let exit_event = PtyExit {
            session_id: session_id_str.clone(),
            exit_code,
            reason,
        };
        if let Err(e) = app_handle.emit("pty-exit", exit_event) {
            eprintln!("[PTY Backend] Failed to emit exit event: {}", e);
        }
    });

    Ok(info)
}

/// Write data to a PTY session
#[tauri::command]
pub async fn write_pty(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
    data: Vec<u8>,
) -> Result<(), String> {

    
    let uuid = Uuid::parse_str(&session_id)
        .map_err(|e| {
            eprintln!("[PTY Backend] Invalid session ID: {}", e);
            format!("Invalid session ID: {}", e)
        })?;

    let mut sessions = state.pty.sessions.lock().await;
    let session = sessions
        .get_mut(&uuid)
        .ok_or_else(|| {
            eprintln!("[PTY Backend] Session not found: {}", session_id);
            "Session not found".to_string()
        })?;

    session
        .write(&data)
        .map_err(|e| format!("Failed to write to PTY: {}", e))?;
    
    Ok(())
}

/// Resize a PTY session
#[tauri::command]
pub async fn resize_pty(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let uuid = Uuid::parse_str(&session_id)
        .map_err(|e| format!("Invalid session ID: {}", e))?;

    let mut sessions = state.pty.sessions.lock().await;
    let session = sessions
        .get_mut(&uuid)
        .ok_or_else(|| "Session not found".to_string())?;

    // Resize the PTY master
    resize_pty_session(session, cols, rows)?;
    
    // Update stored dimensions
    session.cols = cols;
    session.rows = rows;
    
    Ok(())
}

/// Kill a PTY session
#[tauri::command]
pub async fn kill_pty(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
) -> Result<(), String> {
    let uuid = Uuid::parse_str(&session_id)
        .map_err(|e| format!("Invalid session ID: {}", e))?;

    let mut sessions = state.pty.sessions.lock().await;
    
    // Remove the session - dropping it will clean up the PTY
    sessions
        .remove(&uuid)
        .ok_or_else(|| "Session not found".to_string())?;
    
    // Remove from store
    if let Err(e) = state.pty.store.remove_session(uuid) {
        eprintln!("[PTY Backend] Failed to remove session from store: {}", e);
    }
    
    // Remove scrollback
    state.pty.scrollback.remove(uuid).await;

    Ok(())
}

/// List all PTY sessions
#[tauri::command]
pub async fn list_pty_sessions(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<PtyInfo>, String> {
    let sessions = state.pty.sessions.lock().await;
    
    let infos: Vec<PtyInfo> = sessions
        .values()
        .map(|session| PtyInfo {
            id: session.id.to_string(),
            name: session.name.clone(),
            cols: session.cols,
            rows: session.rows,
            is_new: false,
            project_id: None, // TODO: store project_id in session
        })
        .collect();

    Ok(infos)
}

/// Get scrollback buffer for a session
#[tauri::command]
pub async fn get_pty_scrollback(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
) -> Result<Vec<u8>, String> {
    let uuid = Uuid::parse_str(&session_id)
        .map_err(|e| format!("Invalid session ID: {}", e))?;
    
    state.pty.scrollback
        .get(uuid)
        .await
        .ok_or_else(|| "Scrollback not found".to_string())
}

/// Write data to all active PTY sessions
#[tauri::command]
pub async fn write_to_all_ptys(
    state: tauri::State<'_, crate::AppState>,
    data: Vec<u8>,
) -> Result<u32, String> {

    
    let mut sessions = state.pty.sessions.lock().await;
    let mut success_count = 0u32;
    
    for (_uuid, session) in sessions.iter_mut() {
        match session.write(&data) {
            Ok(_) => {
                success_count += 1;
            }
            Err(_e) => {
                // Failed to write to this session, continue with others
            }
        }
    }
    Ok(success_count)
}

/// Attach to an existing PTY session (update last attached time)
#[tauri::command]
pub async fn attach_pty_session(
    state: tauri::State<'_, crate::AppState>,
    session_id: String,
) -> Result<PtyInfo, String> {
    let uuid = Uuid::parse_str(&session_id)
        .map_err(|e| format!("Invalid session ID: {}", e))?;
    
    // Update last attached time
    if let Err(e) = state.pty.store.mark_alive(uuid) {
        eprintln!("[PTY Backend] Failed to mark session alive: {}", e);
    }
    
    let sessions = state.pty.sessions.lock().await;
    let session = sessions
        .get(&uuid)
        .ok_or_else(|| "Session not found".to_string())?;
    
    Ok(PtyInfo {
        id: session.id.to_string(),
        name: session.name.clone(),
        cols: session.cols,
        rows: session.rows,
        is_new: false,
        project_id: None,
    })
}
