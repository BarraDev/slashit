use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::domain::{AgentExecution, Project, Task};

pub struct IpcContext {
    pub tasks: Arc<RwLock<HashMap<Uuid, Task>>>,
    pub projects: Arc<RwLock<HashMap<Uuid, Project>>>,
    pub executions: Arc<RwLock<HashMap<Uuid, AgentExecution>>>,
    pub pty: crate::pty::PtyState,
    pub queue_manager: Arc<RwLock<crate::queue::QueueManager>>,
    pub storage: crate::config::Storage,
    pub app_handle: tauri::AppHandle,
}

pub async fn run(ctx: IpcContext) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = slashit_ipc::socket_path();

    // Remove stale socket file
    std::fs::remove_file(&path).ok();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let listener = UnixListener::bind(&path)?;

    // Set socket permissions to 0o600 (owner-only read/write)
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }

    println!("SlashIt: IPC server listening on {}", path.display());

    // Wrap context in Arc for sharing across connection tasks
    let ctx = Arc::new(ctx);

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, &ctx).await {
                        eprintln!("SlashIt: IPC connection error: {}", e);
                    }
                });
            }
            Err(e) => {
                eprintln!("SlashIt: IPC accept error: {}", e);
            }
        }
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    ctx: &IpcContext,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    buf_reader.read_line(&mut line).await?;

    let response = match serde_json::from_str::<slashit_ipc::IpcRequest>(&line) {
        Ok(request) => super::handlers::dispatch(request, ctx).await,
        Err(e) => slashit_ipc::IpcResponse::error(format!("Invalid request: {}", e)),
    };

    let mut json = serde_json::to_string(&response)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;

    Ok(())
}
