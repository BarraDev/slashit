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
///
/// Only creates when nothing exists yet at `dir` — an already-existing entry
/// (from `$XDG_RUNTIME_DIR/slashit-app`, or a leftover in the world-writable
/// temp-dir fallback) is never chmod'd on trust alone; it goes through
/// [`ensure_safe_runtime_dir`] first, the same as a freshly created one.
fn prepare_socket_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    ensure_safe_runtime_dir(dir)
}

/// Refuse to trust a pre-existing runtime directory just because something is
/// there. `create_dir`-then-chmod only covers a directory this process just
/// made; a directory that already existed could be a symlink planted to
/// redirect the chmod (and the eventual socket) somewhere else, a leftover
/// owned by a different user, or one left in an over-permissive mode by an
/// older SlashIt version. This is the single authoritative check for both the
/// `$XDG_RUNTIME_DIR` and temp-dir-fallback cases — both funnel through
/// [`prepare_socket_dir`], so the invariant only needs to live once.
fn ensure_safe_runtime_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::io::Error;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let meta = std::fs::symlink_metadata(dir)?;
    if meta.file_type().is_symlink() {
        return Err(Error::other(format!(
            "refusing to use {}: it is a symlink, not a real directory",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(Error::other(format!(
            "refusing to use {}: not a directory",
            dir.display()
        )));
    }
    if meta.uid() != current_uid() {
        return Err(Error::other(format!(
            "refusing to use {}: not owned by the current user",
            dir.display()
        )));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn current_uid() -> u32 {
    // No `libc` dependency in this crate; declaring the one symbol needed
    // avoids pulling one in just for this.
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
    // Safety: geteuid takes no arguments and always succeeds.
    unsafe { libc_geteuid() }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn prepare_socket_dir_creates_a_fresh_directory_at_0700() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("runtime");

        prepare_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn prepare_socket_dir_accepts_and_tightens_an_existing_owned_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("runtime");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        prepare_socket_dir(&dir).unwrap();

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "an over-permissive but owned directory is tightened, not rejected");
    }

    #[test]
    fn prepare_socket_dir_refuses_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("elsewhere");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("runtime");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = prepare_socket_dir(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[test]
    fn prepare_socket_dir_refuses_a_regular_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("runtime");
        std::fs::write(&path, b"not a directory").unwrap();

        let err = prepare_socket_dir(&path).unwrap_err();
        assert!(err.to_string().contains("not a directory"));
    }

    // Rejecting a directory owned by a different uid is exercised by
    // `ensure_safe_runtime_dir`'s `meta.uid() != current_uid()` check, but a
    // unit test cannot fabricate a directory owned by another user without
    // root — that branch is verified by code inspection only.
}
