//! PTY module tests
//! Run with: cargo test -p slashit-ui

use super::*;
use super::store::ScrollbackBuffer;

#[cfg(test)]
mod scrollback_tests {
    use super::*;

    #[test]
    fn test_scrollback_buffer_new() {
        let buffer = ScrollbackBuffer::new(1000);
        assert_eq!(buffer.get_data().len(), 0);
    }

    #[test]
    fn test_scrollback_buffer_append() {
        let mut buffer = ScrollbackBuffer::new(1000);
        buffer.append(b"Hello");
        buffer.append(b" World");
        
        assert_eq!(buffer.get_data(), b"Hello World");
    }

    #[test]
    fn test_scrollback_buffer_max_size() {
        let mut buffer = ScrollbackBuffer::new(10);
        buffer.append(b"Hello World!"); // 12 bytes, exceeds max
        
        // Should be truncated to last 10 bytes
        assert!(buffer.get_data().len() <= 10);
    }
}

#[cfg(test)]
mod pty_exit_tests {
    use super::*;

    #[test]
    fn test_pty_exit_struct() {
        let exit = PtyExit {
            session_id: "test-session".to_string(),
            exit_code: Some(0),
            reason: "Shell exited normally".to_string(),
        };
        
        assert_eq!(exit.exit_code, Some(0));
        assert_eq!(exit.reason, "Shell exited normally");
    }

    #[test]
    fn test_pty_exit_with_error() {
        let exit = PtyExit {
            session_id: "test-session".to_string(),
            exit_code: Some(1),
            reason: "Error: Connection reset".to_string(),
        };
        
        assert_eq!(exit.exit_code, Some(1));
        assert!(exit.reason.contains("Error"));
    }
}

#[cfg(test)]
mod pty_info_tests {
    use super::*;

    #[test]
    fn test_pty_info_creation() {
        let info = PtyInfo {
            id: "abc-123".to_string(),
            name: "Terminal 1".to_string(),
            cols: 80,
            rows: 24,
            is_new: true,
            project_id: None,
        };
        
        assert_eq!(info.cols, 80);
        assert_eq!(info.rows, 24);
        assert!(info.is_new);
    }
}

#[cfg(test)]
mod pty_output_tests {
    use super::*;

    #[test]
    fn test_pty_output_creation() {
        let output = PtyOutput {
            session_id: "test-session".to_string(),
            data: vec![0x1b, b'[', b'H'], // ESC [ H - cursor home
        };
        
        assert_eq!(output.data.len(), 3);
        assert_eq!(output.data[0], 0x1b);
    }
}

// ==================== PTY Manager Tests ====================

#[cfg(test)]
mod pty_manager_tests {
    use super::manager::PtyState;

    #[test]
    fn test_pty_state_new() {
        let state = PtyState::new();
        // Verify state is created successfully
        // Sessions should be empty on init
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let sessions = state.sessions.lock().await;
            assert!(sessions.is_empty(), "Sessions should be empty on init");
        });
    }

    #[test]
    fn test_pty_state_default() {
        let state = PtyState::default();
        // Default should be same as new()
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let sessions = state.sessions.lock().await;
            assert!(sessions.is_empty(), "Sessions should be empty on default");
        });
    }
}

#[cfg(test)]
mod pty_spawn_tests {
    use super::manager::spawn_pty_session;

    /// Test that spawn_pty_session returns a valid session on success
    /// This is an integration test that actually spawns a shell process
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_spawn_pty_session_success() {
        let result = spawn_pty_session(
            "Test Terminal".to_string(),
            80,
            24,
            None,
        );
        
        assert!(result.is_ok(), "PTY spawn should succeed: {:?}", result.err());
        
        let (session, _reader) = result.unwrap();
        assert_eq!(session.name, "Test Terminal");
        assert_eq!(session.cols, 80);
        assert_eq!(session.rows, 24);
    }

    /// Test spawn with custom dimensions
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_spawn_pty_session_custom_dimensions() {
        let result = spawn_pty_session(
            "Wide Terminal".to_string(),
            120,
            40,
            None,
        );
        
        assert!(result.is_ok(), "PTY spawn should succeed with custom dimensions");
        
        let (session, _reader) = result.unwrap();
        assert_eq!(session.cols, 120);
        assert_eq!(session.rows, 40);
    }

    /// Test spawn with working directory
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_spawn_pty_session_with_working_directory() {
        // Use a known existing directory (temp dir on all platforms)
        let temp_dir = std::env::temp_dir();
        let result = spawn_pty_session(
            "Terminal with WD".to_string(),
            80,
            24,
            Some(temp_dir.to_string_lossy().to_string()),
        );
        
        assert!(result.is_ok(), "PTY spawn should succeed with working directory");
    }

    /// Test spawn with non-existent working directory falls back gracefully
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_spawn_pty_session_invalid_working_directory() {
        let result = spawn_pty_session(
            "Terminal with invalid WD".to_string(),
            80,
            24,
            Some("/this/path/definitely/does/not/exist/12345".to_string()),
        );
        
        // Should still succeed, falling back to default directory
        assert!(result.is_ok(), "PTY spawn should succeed even with invalid working directory");
    }

    /// Test minimum valid dimensions
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_spawn_pty_session_minimum_dimensions() {
        let result = spawn_pty_session(
            "Tiny Terminal".to_string(),
            1,
            1,
            None,
        );
        
        // Even with tiny dimensions, spawn should succeed
        assert!(result.is_ok(), "PTY spawn should succeed with minimum dimensions");
    }
}

// ==================== Retry Logic Constants Tests ====================

/// These tests document and verify the retry logic parameters
/// The actual retry logic is in the frontend, but we test the expected behavior here
#[cfg(test)]
mod retry_logic_tests {
    /// Expected maximum number of retry attempts
    const EXPECTED_MAX_RETRIES: u32 = 3;
    
    /// Expected base delay for exponential backoff (milliseconds)
    const EXPECTED_BASE_DELAY_MS: u32 = 200;
    
    /// Expected timeout per spawn attempt (milliseconds)
    const EXPECTED_SPAWN_TIMEOUT_MS: u32 = 5000;

    // Compile-time bounds checks on PTY retry tuning. If these constants drift
    // outside the expected range, the crate fails to build with the message
    // baked into the const assert — no runtime test needed.
    const _RETRY_COUNT_OK: () = assert!(
        EXPECTED_MAX_RETRIES >= 2 && EXPECTED_MAX_RETRIES <= 5,
        "Retry count should be between 2 and 5"
    );
    const _BASE_DELAY_OK: () = assert!(
        EXPECTED_BASE_DELAY_MS >= 100 && EXPECTED_BASE_DELAY_MS <= 1000,
        "Base delay should be between 100ms and 1000ms"
    );
    const _SPAWN_TIMEOUT_OK: () = assert!(
        EXPECTED_SPAWN_TIMEOUT_MS >= 3000 && EXPECTED_SPAWN_TIMEOUT_MS <= 10000,
        "Spawn timeout should be between 3s and 10s"
    );

    #[test]
    fn test_total_max_wait_time() {
        // Calculate maximum total wait time with exponential backoff
        // Attempt 1: immediate
        // Attempt 2: 200ms delay
        // Attempt 3: 400ms delay
        // Plus timeouts: 3 * 5000ms = 15000ms
        let total_delay = EXPECTED_BASE_DELAY_MS + (EXPECTED_BASE_DELAY_MS * 2);
        let total_timeout = EXPECTED_MAX_RETRIES * EXPECTED_SPAWN_TIMEOUT_MS;
        let max_wait_time = total_delay + total_timeout;
        
        // Maximum wait time should not exceed 30 seconds
        assert!(
            max_wait_time <= 30000,
            "Maximum wait time should not exceed 30s, got {}ms",
            max_wait_time
        );
    }

    #[test]
    fn test_exponential_backoff_delays() {
        // Verify exponential backoff calculation: 200ms, 400ms, 800ms...
        let delays: Vec<u32> = (0..EXPECTED_MAX_RETRIES)
            .map(|attempt| {
                if attempt == 0 {
                    0 // First attempt has no delay
                } else {
                    EXPECTED_BASE_DELAY_MS * (1 << (attempt - 1))
                }
            })
            .collect();
        
        assert_eq!(delays, vec![0, 200, 400], "Exponential backoff should be 0, 200ms, 400ms");
    }
}

// ==================== Session Resize Tests ====================

#[cfg(test)]
mod pty_resize_tests {
    use super::manager::spawn_pty_session;

    /// Test that PTY session can be resized after creation
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_pty_session_resize() {
        let result = spawn_pty_session(
            "Resizable Terminal".to_string(),
            80,
            24,
            None,
        );
        
        assert!(result.is_ok());
        let (session, _reader) = result.unwrap();
        
        // Resize the session
        let resize_result = session.resize(120, 40);
        assert!(resize_result.is_ok(), "Resize should succeed: {:?}", resize_result.err());
    }

    /// Test resize with same dimensions (no-op)
    #[test]
    #[ignore] // Ignore by default as it spawns real processes
    fn test_pty_session_resize_same_size() {
        let result = spawn_pty_session(
            "Terminal".to_string(),
            80,
            24,
            None,
        );
        
        assert!(result.is_ok());
        let (session, _reader) = result.unwrap();
        
        // Resize to same dimensions should be fine
        let resize_result = session.resize(80, 24);
        assert!(resize_result.is_ok());
    }
}

#[cfg(test)]
mod project_scope_tests {
    use super::super::commands::project_takes_input;

    #[test]
    fn test_project_takes_input_only_its_own_sessions() {
        assert!(project_takes_input(Some("project-a"), Some("project-a")));
        assert!(!project_takes_input(Some("project-b"), Some("project-a")));
        // A session opened outside any Project is not reachable from one.
        assert!(!project_takes_input(None, Some("project-a")));
    }

    #[test]
    fn test_project_takes_input_without_a_project_takes_every_session() {
        assert!(project_takes_input(Some("project-a"), None));
        assert!(project_takes_input(None, None));
    }
}

/// Linux only: liveness is read from `/proc`, which other Unix systems do not
/// have. Elsewhere every pid would read as gone, and the survivor checks would
/// pass without checking anything.
#[cfg(all(test, target_os = "linux"))]
mod quit_tests {
    use crate::pty::manager::spawn_pty_session;
    use std::time::{Duration, Instant};

    /// `SHELL` is process-wide, and the tests that set it must not interleave:
    /// one test's script directory is removed while the other spawns.
    static SHELL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the environment lock every test that mutates the environment
    /// shares, then this module's `SHELL` lock. Every test here sets `SHELL`.
    fn lock_shell() -> (tokio::sync::MutexGuard<'static, ()>, std::sync::MutexGuard<'static, ()>) {
        let env = crate::config::paths::ENV_LOCK.blocking_lock();
        let shell = SHELL_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (env, shell)
    }

    /// Whether `pid` is a live process. A zombie is not live: it has ended and
    /// only waits to be reaped.
    fn is_live(pid: i32) -> bool {
        // Safety: signal 0 sends nothing; it only asks whether the pid exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !matches!(
                stat.rsplit(')').next().and_then(|rest| rest.trim_start().chars().next()),
                Some('Z' | 'X')
            ),
            Err(_) => false,
        }
    }

    /// Reproduces T5: ending a terminal session leaves a shell that ignores
    /// SIGHUP, and the background job it started, running. The shell and its
    /// child are the processes a person would expect to end with the window.
    ///
    /// `SHELL` is read when the session spawns, so this test changes it for
    /// its own process. Run it alone:
    /// `cargo test -p slashit-ui --lib dropping_a_session -- --ignored --exact`
    #[test]
    #[ignore = "spawns a shell and changes SHELL for this process; run alone with --ignored --exact"]
    fn dropping_a_session_ends_a_shell_that_ignores_hangup_and_its_children() {
        use std::os::unix::fs::PermissionsExt;
        let _locks = lock_shell();

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hup-ignoring-shell.sh");
        let pids = dir.path().join("pids");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntrap '' HUP\nsleep 300 &\necho $$ $! > {}\nwait\n", pids.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("SHELL", &script);

        let (session, reader) = spawn_pty_session(
            "quit-test".to_string(),
            80,
            24,
            Some(dir.path().to_string_lossy().into_owned()),
        )
        .expect("spawn the session");

        let deadline = Instant::now() + Duration::from_secs(10);
        let (shell, child) = loop {
            let parsed = std::fs::read_to_string(&pids).ok().and_then(|text| {
                let mut parts = text.split_whitespace().map(|p| p.parse::<i32>());
                match (parts.next(), parts.next()) {
                    (Some(Ok(shell)), Some(Ok(child))) => Some((shell, child)),
                    _ => None,
                }
            });
            if let Some(pair) = parsed {
                break pair;
            }
            assert!(Instant::now() < deadline, "the shell did not start");
            std::thread::sleep(Duration::from_millis(50));
        };

        drop(session);
        drop(reader);
        std::thread::sleep(Duration::from_secs(2));

        let survivors: Vec<i32> = [shell, child].into_iter().filter(|pid| is_live(*pid)).collect();
        for pid in &survivors {
            // Safety: the pids were started by this test and are only killed here.
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        assert!(survivors.is_empty(), "dropping the session left {survivors:?} running");
    }

    /// The application exits as soon as shutdown returns, so the shutdown path
    /// must have ended the shell's group by then. Unlike the drop above, this
    /// checks with no grace period.
    #[test]
    #[ignore = "spawns a shell and changes SHELL for this process; run alone with --ignored --exact"]
    fn ending_the_sessions_at_shutdown_has_ended_a_shell_that_ignores_hangup_when_it_returns() {
        use std::os::unix::fs::PermissionsExt;
        let _locks = lock_shell();

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hup-ignoring-shell.sh");
        let pids = dir.path().join("pids");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntrap '' HUP\nsleep 300 &\necho $$ $! > {}\nwait\n", pids.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("SHELL", &script);

        let (session, reader) = spawn_pty_session(
            "shutdown-test".to_string(),
            80,
            24,
            Some(dir.path().to_string_lossy().into_owned()),
        )
        .expect("spawn the session");

        let deadline = Instant::now() + Duration::from_secs(10);
        let (shell, child) = loop {
            let parsed = std::fs::read_to_string(&pids).ok().and_then(|text| {
                let mut parts = text.split_whitespace().map(|p| p.parse::<i32>());
                match (parts.next(), parts.next()) {
                    (Some(Ok(shell)), Some(Ok(child))) => Some((shell, child)),
                    _ => None,
                }
            });
            if let Some(pair) = parsed {
                break pair;
            }
            assert!(Instant::now() < deadline, "the shell did not start");
            std::thread::sleep(Duration::from_millis(50));
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(crate::pty::manager::end_sessions(vec![session]));
        drop(reader);

        let survivors: Vec<i32> = [shell, child].into_iter().filter(|pid| is_live(*pid)).collect();
        for pid in &survivors {
            // Safety: the pids were started by this test and are only killed here.
            unsafe { libc::kill(*pid, libc::SIGKILL) };
        }
        assert!(survivors.is_empty(), "ending the sessions at shutdown left {survivors:?} running");
    }

    /// Starts a real interactive bash, which runs a background job and then
    /// exits on its own. Returns the shell's pid, the job's pid and the session.
    /// The shell has exited by the time this returns, and the job still runs.
    fn exited_shell_with_background_job(
        name: &str,
    ) -> (i32, i32, crate::pty::manager::PtySession, Box<dyn std::io::Read + Send>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        std::env::set_var("SHELL", "/bin/bash");
        let (mut session, reader) = spawn_pty_session(
            name.to_string(),
            80,
            24,
            Some(dir.path().to_string_lossy().into_owned()),
        )
        .expect("spawn the session");

        session
            .write(format!("sleep 300 & echo $$ $! > {}; exit\n", pids.display()).as_bytes())
            .expect("write the command");

        let deadline = Instant::now() + Duration::from_secs(10);
        let (shell, child) = loop {
            let parsed = std::fs::read_to_string(&pids).ok().and_then(|text| {
                let mut parts = text.split_whitespace().map(|p| p.parse::<i32>());
                match (parts.next(), parts.next()) {
                    (Some(Ok(shell)), Some(Ok(child))) => Some((shell, child)),
                    _ => None,
                }
            });
            if let Some(pair) = parsed {
                break pair;
            }
            assert!(Instant::now() < deadline, "the shell did not start");
            std::thread::sleep(Duration::from_millis(50));
        };

        while is_live(shell) {
            assert!(Instant::now() < deadline, "the shell did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(is_live(child), "the background job should outlive its shell before the terminal is closed");
        (shell, child, session, reader, dir)
    }

    /// Open defect: the shell exits by itself while its background job runs.
    /// Closing the terminal afterwards should end that job too. Job control
    /// puts the job in its own process group, and the session signals only the
    /// shell's group, so the job survives. Run it alone with
    /// `cargo test -p slashit-ui --lib closing_a_terminal_whose_shell -- --ignored`.
    #[test]
    #[ignore = "open defect: a background job outlives its exited shell; spawns an interactive bash"]
    fn closing_a_terminal_whose_shell_exited_ends_its_background_job() {
        let _locks = lock_shell();
        let (_shell_pid, child, session, reader, _dir) = exited_shell_with_background_job("closed-after-exit");

        drop(session);
        drop(reader);
        std::thread::sleep(Duration::from_secs(2));

        let alive = is_live(child);
        if alive {
            // Safety: the pid was started by this test and is only killed here.
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        assert!(!alive, "closing the terminal left its background job {child} running");
    }

    /// Open defect, at application quit: the same job survives `end_sessions`,
    /// because the shell that has already exited is no longer signalled.
    #[test]
    #[ignore = "open defect: a background job outlives its exited shell; spawns an interactive bash"]
    fn quitting_ends_the_background_job_of_a_shell_that_already_exited() {
        let _locks = lock_shell();
        let (_shell_pid, child, session, reader, _dir) = exited_shell_with_background_job("quit-after-exit");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(crate::pty::manager::end_sessions(vec![session]));
        drop(reader);

        let alive = is_live(child);
        if alive {
            // Safety: the pid was started by this test and is only killed here.
            unsafe { libc::kill(child, libc::SIGKILL) };
        }
        assert!(!alive, "quitting left the background job {child} of an exited shell running");
    }
}
