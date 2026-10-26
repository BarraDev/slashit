use portable_pty::{native_pty_system, CommandBuilder, PtySize, MasterPty, Child};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::store::{ScrollbackManager, SessionMetadata, SessionStore};

/// Information about a PTY session
pub struct PtySession {
    pub id: Uuid,
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    /// The Project the terminal was opened in. Set by the spawn command, and
    /// the only thing that scopes a session to a Project.
    pub project_id: Option<String>,
    writer: Box<dyn Write + Send>,
    // Keep the master PTY alive for resizing and proper operation
    master: Box<dyn MasterPty + Send>,
    // We keep the child to prevent it from being dropped (intentionally unused)
    _child: Box<dyn Child + Send + Sync>,
}

/// Ends a terminal's process group: SIGHUP first, then SIGKILL once the group
/// has had a moment to exit. Closing the master only hangs up the shell, so a
/// shell that ignores SIGHUP, or a job it started, would otherwise outlive it.
#[cfg(unix)]
fn end_process_group(pgid: i32) {
    // Safety: the group is the one this process's own terminal shell leads
    // (portable-pty starts the shell in a new session).
    unsafe { libc::killpg(pgid, libc::SIGHUP) };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        // A group id is not reused while any process still carries it, so a
        // group that is still there is still this terminal's.
        // Safety: as above; signal 0 only probes.
        if unsafe { libc::kill(-pgid, 0) } == 0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    });
}

/// Whether any process still belongs to the group.
#[cfg(unix)]
fn group_is_running(pgid: i32) -> bool {
    // Safety: signal 0 only probes.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Reaps exited shells and waits until none of `groups` is running, or until
/// `limit` has passed. Reaping matters: an exited shell stays a zombie, which
/// keeps its group non-empty until it is collected.
#[cfg(unix)]
async fn wait_for_groups(sessions: &mut [PtySession], groups: &[i32], limit: std::time::Duration, poll: std::time::Duration) {
    let deadline = std::time::Instant::now() + limit;
    loop {
        for session in sessions.iter_mut() {
            let _ = session._child.try_wait();
        }
        if !groups.iter().any(|&pgid| group_is_running(pgid)) || std::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(poll).await;
    }
}

/// Ends every terminal at application shutdown, and returns once no shell
/// group is left or the SIGKILL fallback has been sent.
///
/// The application exits as soon as this returns, so the fallback cannot wait
/// on a detached thread the way dropping a session does: a shell that ignores
/// SIGHUP would then outlive a quit whose dialog said it would end it.
#[cfg(unix)]
pub async fn end_sessions(mut sessions: Vec<PtySession>) {
    use std::time::Duration;
    const POLL: Duration = Duration::from_millis(25);
    const HANGUP_GRACE: Duration = Duration::from_millis(500);
    const KILL_GRACE: Duration = Duration::from_secs(1);

    let groups: Vec<i32> = sessions.iter_mut().filter_map(PtySession::live_group).collect();
    for &pgid in &groups {
        // Safety: each group is one this process's own terminal shell leads.
        unsafe { libc::killpg(pgid, libc::SIGHUP) };
    }
    wait_for_groups(&mut sessions, &groups, HANGUP_GRACE, POLL).await;
    for &pgid in &groups {
        if group_is_running(pgid) {
            // Safety: as above.
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
    wait_for_groups(&mut sessions, &groups, KILL_GRACE, POLL).await;
}

/// Windows has no process groups to signal; dropping the sessions closes them.
#[cfg(not(unix))]
pub async fn end_sessions(sessions: Vec<PtySession>) {
    drop(sessions);
}

impl Drop for PtySession {
    /// Ending the session ends its shell and the jobs it started. A shell that
    /// has already exited is not signalled: once reaped, its pid may name a
    /// different process group.
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if matches!(self._child.try_wait(), Ok(None)) {
                if let Some(pid) = self._child.process_id() {
                    end_process_group(pid as i32);
                }
            }
        }
    }
}

impl PtySession {
    /// The process group of this session's shell, while the shell is running.
    #[cfg(unix)]
    fn live_group(&mut self) -> Option<i32> {
        if matches!(self._child.try_wait(), Ok(None)) {
            self._child.process_id().map(|pid| pid as i32)
        } else {
            None
        }
    }

    /// Write data to the PTY
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }
    
    /// Resize the PTY
    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
    
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }).map_err(|e| format!("Failed to resize: {}", e))
    }
}

/// State for managing PTY sessions
#[derive(Clone)]
pub struct PtyState {
    pub sessions: Arc<Mutex<HashMap<Uuid, PtySession>>>,
    pub scrollback: ScrollbackManager,
    pub store: Arc<SessionStore>,
}

impl PtyState {
    pub fn new() -> Self {
        let store = SessionStore::new().expect("Failed to create session store");
        // Mark all sessions as dead on startup (they'll be revived when spawned)
        let _ = store.mark_all_dead();
        
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            scrollback: ScrollbackManager::new(),
            store: Arc::new(store),
        }
    }

    /// Publish a spawned terminal, and only after its record is written. If the
    /// record cannot be written, nothing is published: the scrollback is
    /// removed, and the session is dropped, which ends its shell.
    pub async fn register_session(&self, session: PtySession, metadata: SessionMetadata) -> Result<(), String> {
        let session_id = session.id;
        self.scrollback.create(session_id).await;
        if let Err(e) = self.store.upsert_session(metadata) {
            self.scrollback.remove(session_id).await;
            return Err(format!("Could not record the terminal session: {e}"));
        }
        self.sessions.lock().await.insert(session_id, session);
        Ok(())
    }
}

impl Default for PtyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the appropriate shell command for the current platform
fn get_shell_command() -> CommandBuilder {
    #[cfg(windows)]
    {
        println!("[PTY Manager] Detecting Windows shell...");
        
        // Prefer pwsh (PowerShell Core 7+) > powershell.exe (Windows PowerShell 5.1) > cmd.exe
        if which::which("pwsh").is_ok() {
            println!("[PTY Manager] Using pwsh.exe (PowerShell Core 7+)");
            let mut cmd = CommandBuilder::new("pwsh.exe");
            cmd.arg("-NoLogo");
            cmd.arg("-NoExit");
            cmd.arg("-Interactive");
            cmd
        } else if std::path::Path::new("C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe").exists() {
            println!("[PTY Manager] Using powershell.exe (Windows PowerShell 5.1)");
            let mut cmd = CommandBuilder::new("powershell.exe");
            cmd.arg("-NoLogo");
            cmd.arg("-NoExit");
            cmd
        } else {
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
            println!("[PTY Manager] Using cmd.exe or COMSPEC: {}", shell);
            let mut cmd = CommandBuilder::new(&shell);
            cmd.arg("/K");  // Keep cmd.exe alive
            cmd
        }
    }
    
    #[cfg(not(windows))]
    {
        // On Unix, use the user's preferred shell with interactive flag
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        println!("[PTY Manager] Using Unix shell: {}", shell);
        let mut cmd = CommandBuilder::new(&shell);
        cmd.arg("-i"); // Interactive mode
        cmd.arg("-l"); // Login shell
        cmd
    }
}

/// Spawn a new PTY session with a shell
pub fn spawn_pty_session(
    name: String,
    cols: u16,
    rows: u16,
    working_directory: Option<String>,
) -> Result<(PtySession, Box<dyn Read + Send>), String> {
    println!("[PTY Manager] === spawn_pty_session START ===");
    println!("[PTY Manager] name={}, cols={}, rows={}, working_directory={:?}", name, cols, rows, working_directory);
    
    println!("[PTY Manager] Step 1: Getting native PTY system...");
    let pty_system = native_pty_system();
    println!("[PTY Manager] Step 1: ✓ Got native PTY system");

    // Open PTY with specified size
    println!("[PTY Manager] Step 2: Opening PTY with size {}x{}...", cols, rows);
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| {
            eprintln!("[PTY Manager] Step 2: ✗ Failed to open PTY: {}", e);
            format!("Failed to open PTY: {}", e)
        })?;
    println!("[PTY Manager] Step 2: ✓ PTY opened successfully");
    


    // Build shell command - platform specific
    println!("[PTY Manager] Step 3: Building shell command...");
    let mut cmd = get_shell_command();
    println!("[PTY Manager] Step 3: ✓ Shell command built");
    
    // Set environment variables for proper terminal emulation
    println!("[PTY Manager] Step 4: Setting environment variables...");
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("LC_ALL", "C.UTF-8");
    println!("[PTY Manager] Step 4: ✓ Environment variables set");
    
    // Set the current working directory
    // Priority: 1) Provided working_directory, 2) Project root detection, 3) Home directory
    println!("[PTY Manager] Step 5: Setting working directory...");
    if let Some(ref wd) = working_directory {
        let path = std::path::PathBuf::from(wd);
        if path.exists() {
            println!("[PTY Manager] Step 5: Using provided working directory: {:?}", path);
            cmd.cwd(path);
        } else {
            println!("[PTY Manager] Step 5: Provided directory doesn't exist, using default");
            set_default_cwd(&mut cmd);
        }
    } else {
        println!("[PTY Manager] Step 5: No working directory provided, using default");
        set_default_cwd(&mut cmd);
    }
    println!("[PTY Manager] Step 5: ✓ Working directory set");


    
    // Spawn the shell process on the slave side
    println!("[PTY Manager] Step 6: Spawning shell process...");
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| {
            eprintln!("[PTY Manager] Step 6: ✗ Failed to spawn shell: {}", e);
            format!("Failed to spawn shell: {}", e)
        })?;
    println!("[PTY Manager] Step 6: ✓ Shell process spawned");
    


    // Get writer for sending input to PTY
    println!("[PTY Manager] Step 7: Getting PTY writer...");
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| {
            eprintln!("[PTY Manager] Step 7: ✗ Failed to get PTY writer: {}", e);
            format!("Failed to get PTY writer: {}", e)
        })?;
    println!("[PTY Manager] Step 7: ✓ PTY writer obtained");
    


    // Get reader for receiving output from PTY
    println!("[PTY Manager] Step 8: Getting PTY reader...");
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| {
            eprintln!("[PTY Manager] Step 8: ✗ Failed to get PTY reader: {}", e);
            format!("Failed to get PTY reader: {}", e)
        })?;
    println!("[PTY Manager] Step 8: ✓ PTY reader obtained");
    


    println!("[PTY Manager] Step 9: Creating PtySession...");
    let session_id = Uuid::new_v4();
    let session = PtySession {
        id: session_id,
        name: name.clone(),
        cols,
        rows,
        project_id: None,
        writer,
        master: pair.master,
        _child: child,
    };
    
    println!("[PTY Manager] Step 9: ✓ PtySession created with id={}", session_id);
    println!("[PTY Manager] === spawn_pty_session SUCCESS ===");

    Ok((session, reader))
}

/// Set default working directory based on project root or home
fn set_default_cwd(cmd: &mut CommandBuilder) {
    // When running via `cargo tauri dev`, current_dir is src-tauri, so we need the parent
    if let Ok(cwd) = std::env::current_dir() {
        // Check if we're in src-tauri and need to go up one level
        let project_root = if cwd.ends_with("src-tauri") {
            cwd.parent().map(|p| p.to_path_buf()).unwrap_or(cwd)
        } else {
            cwd
        };
        cmd.cwd(project_root);
    } else if let Some(home) = dirs::home_dir() {
        // Fallback to home directory if current_dir fails
        cmd.cwd(home);
    }
}

/// Resize an existing PTY session
pub fn resize_pty_session(
    session: &PtySession,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    session.resize(cols, rows)
}
