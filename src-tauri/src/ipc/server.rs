use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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

/// Create the socket directory with owner-only permissions.
///
/// The directory is the real access control. Permissions on the socket file
/// itself can only be applied *after* `bind()` returns, which leaves a window
/// in which the socket exists at its final path with the process umask; a
/// 0700 parent closes that window because nobody else can traverse into it.
fn prepare_socket_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Decide whether an existing socket file may be replaced.
///
/// A socket left behind by a crash must be cleared, but one belonging to a
/// live instance must not: removing it and binding again silently steals every
/// subsequent CLI connection from the running app. Connecting is the only
/// reliable liveness test — a bound socket accepts, an orphaned inode refuses.
async fn claim_socket(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !path.exists() {
        return Ok(());
    }

    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => Err(format!(
            "another SlashIt instance is already listening on {}",
            path.display()
        )
        .into()),
        Err(_) => {
            std::fs::remove_file(path)?;
            Ok(())
        }
    }
}

pub async fn run(ctx: IpcContext) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = slashit_ipc::socket_path();

    if let Some(parent) = path.parent() {
        prepare_socket_dir(parent)?;
    }

    claim_socket(&path).await?;

    let listener = UnixListener::bind(&path)?;

    // Belt and braces: the 0700 directory already prevents access, but a
    // 0600 socket keeps the guarantee if the directory is ever relaxed.
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
    // Cap the read. A client that connects and never sends a newline would
    // otherwise make the server buffer without bound.
    let mut buf_reader = BufReader::new(reader.take(slashit_ipc::MAX_REQUEST_BYTES));
    let mut line = String::new();

    let read = buf_reader.read_line(&mut line).await?;

    let response = if read as u64 >= slashit_ipc::MAX_REQUEST_BYTES {
        slashit_ipc::IpcResponse::error(format!(
            "Request exceeds the {} byte limit",
            slashit_ipc::MAX_REQUEST_BYTES
        ))
    } else {
        match serde_json::from_str::<slashit_ipc::IpcRequest>(&line) {
            Ok(request) => super::handlers::dispatch(request, ctx).await,
            Err(e) => slashit_ipc::IpcResponse::error(format!("Invalid request: {}", e)),
        }
    };

    let mut json = serde_json::to_string(&response)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;

    Ok(())
}
