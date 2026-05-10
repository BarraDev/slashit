//! PTY module tests
//! Run with: cargo test -p slashit-app

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

    #[test]
    fn test_retry_parameters_are_reasonable() {
        // Verify retry count is reasonable (not too few, not too many)
        assert!(
            EXPECTED_MAX_RETRIES >= 2 && EXPECTED_MAX_RETRIES <= 5,
            "Retry count should be between 2 and 5, got {}",
            EXPECTED_MAX_RETRIES
        );
    }

    #[test]
    fn test_base_delay_is_reasonable() {
        // Base delay should be short enough for good UX but long enough to be meaningful
        assert!(
            EXPECTED_BASE_DELAY_MS >= 100 && EXPECTED_BASE_DELAY_MS <= 1000,
            "Base delay should be between 100ms and 1000ms, got {}ms",
            EXPECTED_BASE_DELAY_MS
        );
    }

    #[test]
    fn test_spawn_timeout_is_reasonable() {
        // Timeout should be long enough for slow systems but not too long
        assert!(
            EXPECTED_SPAWN_TIMEOUT_MS >= 3000 && EXPECTED_SPAWN_TIMEOUT_MS <= 10000,
            "Spawn timeout should be between 3s and 10s, got {}ms",
            EXPECTED_SPAWN_TIMEOUT_MS
        );
    }

    #[test]
    fn test_total_max_wait_time() {
        // Calculate maximum total wait time with exponential backoff
        // Attempt 1: immediate
        // Attempt 2: 200ms delay
        // Attempt 3: 400ms delay
        // Plus timeouts: 3 * 5000ms = 15000ms
        let total_delay = 0 + EXPECTED_BASE_DELAY_MS + (EXPECTED_BASE_DELAY_MS * 2);
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
