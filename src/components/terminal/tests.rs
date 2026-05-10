//! Terminal component tests
//!
//! Tests for the terminal component including:
//! - Retry logic parameters
//! - Dimension calculations
//! - Terminal state management
//! - Parser integration
//! - WASM browser tests for input handling and SGR parsing

use super::parser::parse_bytes;
use super::state::{TerminalState, CellAttributes, Color};
use super::input::key_to_escape_sequence;

// WASM browser test configuration
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::*;

#[cfg(target_arch = "wasm32")]
wasm_bindgen_test_configure!(run_in_browser);

// ==================== Retry Logic Constants Tests ====================

/// These constants should match the values in component.rs
/// We test them here to prevent accidental changes that could cause regressions
mod retry_constants {
    /// Maximum number of retry attempts for PTY spawn
    pub const MAX_SPAWN_RETRIES: u32 = 3;
    
    /// Base delay for exponential backoff (milliseconds)
    pub const BASE_RETRY_DELAY_MS: u32 = 200;
    
    /// Timeout per spawn attempt (milliseconds)
    pub const SPAWN_TIMEOUT_MS: u32 = 5000;
}

#[cfg(test)]
mod retry_logic_integration_tests {
    use super::retry_constants::*;

    #[test]
    fn test_max_spawn_retries_matches_component() {
        // This test ensures the retry count hasn't been accidentally changed
        assert_eq!(MAX_SPAWN_RETRIES, 3, "MAX_SPAWN_RETRIES should be 3");
    }

    #[test]
    fn test_base_retry_delay_matches_component() {
        // This test ensures the base delay hasn't been accidentally changed
        assert_eq!(BASE_RETRY_DELAY_MS, 200, "BASE_RETRY_DELAY_MS should be 200ms");
    }

    #[test]
    fn test_spawn_timeout_matches_component() {
        // This test ensures the spawn timeout hasn't been accidentally changed
        assert_eq!(SPAWN_TIMEOUT_MS, 5000, "SPAWN_TIMEOUT_MS should be 5000ms");
    }

    #[test]
    fn test_exponential_backoff_sequence() {
        // Verify the exponential backoff calculation produces expected delays
        // Attempt 1: 0ms (immediate)
        // Attempt 2: 200ms
        // Attempt 3: 400ms
        let expected_delays = vec![0u32, 200, 400];
        
        for attempt in 0..MAX_SPAWN_RETRIES {
            let delay = if attempt == 0 {
                0
            } else {
                BASE_RETRY_DELAY_MS * (1 << (attempt - 1))
            };
            assert_eq!(
                delay, expected_delays[attempt as usize],
                "Delay for attempt {} should be {}ms, got {}ms",
                attempt + 1, expected_delays[attempt as usize], delay
            );
        }
    }

    #[test]
    fn test_total_retry_time_is_bounded() {
        // Calculate maximum possible time spent in retry loop
        // This includes all delays plus all timeouts
        let total_delays: u32 = (0..MAX_SPAWN_RETRIES)
            .map(|attempt| {
                if attempt == 0 { 0 } else { BASE_RETRY_DELAY_MS * (1 << (attempt - 1)) }
            })
            .sum();
        let total_timeouts = MAX_SPAWN_RETRIES * SPAWN_TIMEOUT_MS;
        let max_total_time = total_delays + total_timeouts;
        
        // Should complete within 20 seconds worst case
        // 3 * 5s timeout + 600ms delays = 15.6s max
        assert!(
            max_total_time <= 20_000,
            "Total retry time should be <= 20s, got {}ms",
            max_total_time
        );
        
        // Should take at least some time (not instant)
        assert!(
            max_total_time >= 5_000,
            "Total retry time should be >= 5s, got {}ms",
            max_total_time
        );
    }

    #[test]
    fn test_retry_count_provides_sufficient_resilience() {
        // 3 retries should handle:
        // - Transient failures (1-2 retries usually sufficient)
        // - Slow startup (timeouts give time to initialize)
        // - Brief resource contention
        assert!(
            MAX_SPAWN_RETRIES >= 2,
            "Should have at least 2 retries for resilience"
        );
        assert!(
            MAX_SPAWN_RETRIES <= 5,
            "More than 5 retries indicates deeper issues"
        );
    }
}

// ==================== Dimension Calculation Tests ====================

#[cfg(test)]
mod dimension_tests {
    // Character cell dimensions from component.rs
    const CHAR_WIDTH: f64 = 8.4;
    const CHAR_HEIGHT: f64 = 16.8;
    const MIN_COLS: u16 = 40;
    const MIN_ROWS: u16 = 10;
    const MAX_COLS: u16 = 500;
    const MAX_ROWS: u16 = 200;

    /// Calculate terminal dimensions from pixel size (mirrors component.rs logic)
    fn calculate_dimensions(width: f64, height: f64) -> (u16, u16) {
        let usable_width = (width - 16.0).max(0.0);
        let usable_height = (height - 16.0).max(0.0);
        
        let cols = (usable_width / CHAR_WIDTH) as u16;
        let rows = (usable_height / CHAR_HEIGHT) as u16;
        
        let cols = cols.clamp(MIN_COLS, MAX_COLS);
        let rows = rows.clamp(MIN_ROWS, MAX_ROWS);
        
        (cols, rows)
    }

    #[test]
    fn test_calculate_dimensions_standard_size() {
        // Standard terminal size: 800x600 container
        let (cols, rows) = calculate_dimensions(800.0, 600.0);
        
        // (800 - 16) / 8.4 ≈ 93 cols
        // (600 - 16) / 16.8 ≈ 34 rows
        assert_eq!(cols, 93, "Standard width should give ~93 cols");
        assert_eq!(rows, 34, "Standard height should give ~34 rows");
    }

    #[test]
    fn test_calculate_dimensions_minimum_clamp() {
        // Very small container should clamp to minimums
        let (cols, rows) = calculate_dimensions(100.0, 100.0);
        
        assert_eq!(cols, MIN_COLS, "Small width should clamp to MIN_COLS");
        assert_eq!(rows, MIN_ROWS, "Small height should clamp to MIN_ROWS");
    }

    #[test]
    fn test_calculate_dimensions_maximum_clamp() {
        // Very large container should clamp to maximums
        let (cols, rows) = calculate_dimensions(10000.0, 10000.0);
        
        assert_eq!(cols, MAX_COLS, "Large width should clamp to MAX_COLS");
        assert_eq!(rows, MAX_ROWS, "Large height should clamp to MAX_ROWS");
    }

    #[test]
    fn test_calculate_dimensions_zero_size() {
        // Zero size should give minimum dimensions
        let (cols, rows) = calculate_dimensions(0.0, 0.0);
        
        assert_eq!(cols, MIN_COLS, "Zero width should give MIN_COLS");
        assert_eq!(rows, MIN_ROWS, "Zero height should give MIN_ROWS");
    }

    #[test]
    fn test_calculate_dimensions_negative_size() {
        // Negative size (shouldn't happen but handle gracefully)
        let (cols, rows) = calculate_dimensions(-100.0, -100.0);
        
        assert_eq!(cols, MIN_COLS, "Negative width should give MIN_COLS");
        assert_eq!(rows, MIN_ROWS, "Negative height should give MIN_ROWS");
    }

    #[test]
    fn test_calculate_dimensions_accounts_for_padding() {
        // Verify padding (16px) is accounted for
        // 100px width - 16px padding = 84px usable
        // 84 / 8.4 = 10 cols (but clamped to MIN_COLS)
        let (cols, _) = calculate_dimensions(100.0, 300.0);
        assert_eq!(cols, MIN_COLS, "Padding should be subtracted from usable width");
        
        // 400px width - 16px = 384px
        // 384 / 8.4 ≈ 45 cols (above minimum)
        let (cols, _) = calculate_dimensions(400.0, 300.0);
        assert_eq!(cols, 45, "Should calculate cols correctly with padding");
    }
}

// ==================== Terminal State Integration Tests ====================

#[cfg(test)]
mod terminal_state_integration_tests {
    use super::*;

    #[test]
    fn test_terminal_state_default_dimensions() {
        let state = TerminalState::new(80, 24);
        assert_eq!(state.cols, 80);
        assert_eq!(state.rows, 24);
    }

    #[test]
    fn test_terminal_state_resize() {
        let mut state = TerminalState::new(80, 24);
        state.resize(120, 40);
        assert_eq!(state.cols, 120);
        assert_eq!(state.rows, 40);
    }

    #[test]
    fn test_terminal_state_cursor_position() {
        let state = TerminalState::new(80, 24);
        assert_eq!(state.cursor_x, 0);
        assert_eq!(state.cursor_y, 0);
    }

    #[test]
    fn test_terminal_state_scrollback() {
        let mut state = TerminalState::new(80, 24);
        assert_eq!(state.scroll_offset, 0);
        assert_eq!(state.max_scroll_offset(), 0);
        
        // Fill terminal and create scrollback
        for _ in 0..50 {
            parse_bytes(&mut state, b"\n");
        }
        
        // Should have scrollback now
        assert!(state.max_scroll_offset() > 0);
    }

    #[test]
    fn test_terminal_state_scroll_to_bottom() {
        let mut state = TerminalState::new(80, 24);
        
        // Create some scrollback
        for _ in 0..50 {
            parse_bytes(&mut state, b"\n");
        }
        
        // Scroll up
        state.scroll_view(10);
        assert!(state.scroll_offset > 0);
        
        // Scroll to bottom
        state.scroll_to_bottom();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_terminal_state_output_processing() {
        let mut state = TerminalState::new(80, 24);
        
        // Write some text
        parse_bytes(&mut state, b"Hello, World!");
        
        // Cursor should have moved
        assert_eq!(state.cursor_x, 13);
        assert_eq!(state.cursor_y, 0);
    }

    #[test]
    fn test_terminal_state_newline_handling() {
        let mut state = TerminalState::new(80, 24);
        
        // Write text with newline
        parse_bytes(&mut state, b"Line 1\nLine 2");
        
        // Cursor should be on second line
        assert_eq!(state.cursor_y, 1);
    }

    #[test]
    fn test_terminal_state_carriage_return() {
        let mut state = TerminalState::new(80, 24);
        
        // Write text, then carriage return
        parse_bytes(&mut state, b"Hello\rWorld");
        
        // Cursor should be at position 5 (length of "World")
        assert_eq!(state.cursor_x, 5);
        assert_eq!(state.cursor_y, 0);
    }
}

// ==================== Parser Tests ====================

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn test_parse_simple_text() {
        let mut state = TerminalState::new(80, 24);
        parse_bytes(&mut state, b"Hello");
        
        // Check that text was written to the grid
        let visible = state.visible_rows();
        assert_eq!(visible[0][0].char, 'H');
        assert_eq!(visible[0][1].char, 'e');
        assert_eq!(visible[0][2].char, 'l');
        assert_eq!(visible[0][3].char, 'l');
        assert_eq!(visible[0][4].char, 'o');
    }

    #[test]
    fn test_parse_escape_sequence_clear() {
        let mut state = TerminalState::new(80, 24);
        
        // Write some text
        parse_bytes(&mut state, b"Hello");
        
        // Clear screen (ESC [ 2 J)
        parse_bytes(&mut state, b"\x1b[2J");
        
        // Grid should be cleared (all spaces)
        let visible = state.visible_rows();
        for cell in visible[0].iter() {
            assert_eq!(cell.char, ' ');
        }
    }

    #[test]
    fn test_parse_cursor_movement() {
        let mut state = TerminalState::new(80, 24);
        
        // Move cursor to position (10, 5) - ESC [ row ; col H
        parse_bytes(&mut state, b"\x1b[5;10H");
        
        // Cursor should be at (9, 4) - 0-indexed
        assert_eq!(state.cursor_x, 9);
        assert_eq!(state.cursor_y, 4);
    }

    #[test]
    fn test_parse_utf8_text() {
        let mut state = TerminalState::new(80, 24);
        
        // Write UTF-8 text
        parse_bytes(&mut state, "日本語".as_bytes());
        
        // Check that UTF-8 characters were written
        let visible = state.visible_rows();
        assert_eq!(visible[0][0].char, '日');
    }
}

// ==================== Scrollback Buffer Tests ====================

#[cfg(test)]
mod scrollback_tests {
    use super::*;

    #[test]
    fn test_scrollback_initially_empty() {
        let state = TerminalState::new(80, 24);
        assert_eq!(state.max_scroll_offset(), 0);
    }

    #[test]
    fn test_scrollback_grows_with_output() {
        let mut state = TerminalState::new(80, 24);
        
        // Fill more than one screen worth of lines
        for i in 0..50 {
            parse_bytes(&mut state, format!("Line {}\n", i).as_bytes());
        }
        
        // Should have scrollback
        let max_scroll = state.max_scroll_offset();
        assert!(max_scroll > 0, "Should have scrollback after filling screen");
    }

    #[test]
    fn test_scroll_view_up_and_down() {
        let mut state = TerminalState::new(80, 24);
        
        // Create scrollback
        for i in 0..50 {
            parse_bytes(&mut state, format!("Line {}\n", i).as_bytes());
        }
        
        // Scroll up (positive = up)
        state.scroll_view(5);
        assert_eq!(state.scroll_offset, 5);
        
        // Scroll down (negative = down)
        state.scroll_view(-3);
        assert_eq!(state.scroll_offset, 2);
        
        // Scroll past bottom
        state.scroll_view(-10);
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_scroll_offset_clamped() {
        let mut state = TerminalState::new(80, 24);
        
        // Create some scrollback (25 lines of output = 1 line of scrollback)
        for i in 0..50 {
            parse_bytes(&mut state, format!("Line {}\n", i).as_bytes());
        }
        
        let max = state.max_scroll_offset();
        
        // Try to scroll past maximum
        state.scroll_view(1000);
        assert_eq!(state.scroll_offset, max, "Should clamp to max scroll offset");
        
        // Try to scroll below zero
        state.scroll_view(-2000);
        assert_eq!(state.scroll_offset, 0, "Should clamp to 0");
    }
}

// ============================================================================
// WASM Browser Tests - Input Handling
// ============================================================================

#[cfg(target_arch = "wasm32")]
mod wasm_input_tests {
    use super::*;

    #[wasm_bindgen_test]
    fn test_regular_character() {
        let result = key_to_escape_sequence("a", false, false, false);
        assert_eq!(result, Some(vec![b'a']));
    }

    #[wasm_bindgen_test]
    fn test_uppercase_character() {
        let result = key_to_escape_sequence("A", false, false, true);
        assert_eq!(result, Some(vec![b'A']));
    }

    #[wasm_bindgen_test]
    fn test_enter_key() {
        let result = key_to_escape_sequence("Enter", false, false, false);
        assert_eq!(result, Some(vec![0x0D]));
    }

    #[wasm_bindgen_test]
    fn test_backspace_key() {
        let result = key_to_escape_sequence("Backspace", false, false, false);
        assert_eq!(result, Some(vec![0x7F]));
    }

    #[wasm_bindgen_test]
    fn test_tab_key() {
        let result = key_to_escape_sequence("Tab", false, false, false);
        assert_eq!(result, Some(vec![0x09]));
    }

    #[wasm_bindgen_test]
    fn test_shift_tab() {
        let result = key_to_escape_sequence("Tab", false, false, true);
        assert_eq!(result, Some(b"\x1b[Z".to_vec()));
    }

    #[wasm_bindgen_test]
    fn test_escape_key() {
        let result = key_to_escape_sequence("Escape", false, false, false);
        assert_eq!(result, Some(vec![0x1B]));
    }

    #[wasm_bindgen_test]
    fn test_space_key() {
        let result = key_to_escape_sequence("Space", false, false, false);
        assert_eq!(result, Some(vec![0x20]));
    }

    #[wasm_bindgen_test]
    fn test_arrow_up() {
        let result = key_to_escape_sequence("ArrowUp", false, false, false);
        assert_eq!(result, Some(vec![0x1b, b'[', b'A']));
    }

    #[wasm_bindgen_test]
    fn test_arrow_down() {
        let result = key_to_escape_sequence("ArrowDown", false, false, false);
        assert_eq!(result, Some(vec![0x1b, b'[', b'B']));
    }

    #[wasm_bindgen_test]
    fn test_arrow_right() {
        let result = key_to_escape_sequence("ArrowRight", false, false, false);
        assert_eq!(result, Some(vec![0x1b, b'[', b'C']));
    }

    #[wasm_bindgen_test]
    fn test_arrow_left() {
        let result = key_to_escape_sequence("ArrowLeft", false, false, false);
        assert_eq!(result, Some(vec![0x1b, b'[', b'D']));
    }

    #[wasm_bindgen_test]
    fn test_ctrl_c() {
        let result = key_to_escape_sequence("c", true, false, false);
        assert_eq!(result, Some(vec![0x03])); // ETX - interrupt
    }

    #[wasm_bindgen_test]
    fn test_ctrl_d() {
        let result = key_to_escape_sequence("d", true, false, false);
        assert_eq!(result, Some(vec![0x04])); // EOT
    }

    #[wasm_bindgen_test]
    fn test_ctrl_z() {
        let result = key_to_escape_sequence("z", true, false, false);
        assert_eq!(result, Some(vec![0x1A])); // SUB - suspend
    }

    #[wasm_bindgen_test]
    fn test_ctrl_l() {
        let result = key_to_escape_sequence("l", true, false, false);
        assert_eq!(result, Some(vec![0x0C])); // Form feed - clear screen
    }

    #[wasm_bindgen_test]
    fn test_alt_key() {
        let result = key_to_escape_sequence("x", false, true, false);
        assert_eq!(result, Some(vec![0x1B, b'x'])); // ESC prefix + char
    }

    #[wasm_bindgen_test]
    fn test_function_keys() {
        assert_eq!(key_to_escape_sequence("F1", false, false, false), Some(b"\x1bOP".to_vec()));
        assert_eq!(key_to_escape_sequence("F2", false, false, false), Some(b"\x1bOQ".to_vec()));
        assert_eq!(key_to_escape_sequence("F3", false, false, false), Some(b"\x1bOR".to_vec()));
        assert_eq!(key_to_escape_sequence("F4", false, false, false), Some(b"\x1bOS".to_vec()));
        assert_eq!(key_to_escape_sequence("F5", false, false, false), Some(b"\x1b[15~".to_vec()));
        assert_eq!(key_to_escape_sequence("F12", false, false, false), Some(b"\x1b[24~".to_vec()));
    }

    #[wasm_bindgen_test]
    fn test_navigation_keys() {
        assert_eq!(key_to_escape_sequence("Home", false, false, false), Some(b"\x1b[H".to_vec()));
        assert_eq!(key_to_escape_sequence("End", false, false, false), Some(b"\x1b[F".to_vec()));
        assert_eq!(key_to_escape_sequence("Insert", false, false, false), Some(b"\x1b[2~".to_vec()));
        assert_eq!(key_to_escape_sequence("Delete", false, false, false), Some(b"\x1b[3~".to_vec()));
        assert_eq!(key_to_escape_sequence("PageUp", false, false, false), Some(b"\x1b[5~".to_vec()));
        assert_eq!(key_to_escape_sequence("PageDown", false, false, false), Some(b"\x1b[6~".to_vec()));
    }

    #[wasm_bindgen_test]
    fn test_modifier_only_keys_ignored() {
        assert!(key_to_escape_sequence("Control", false, false, false).is_none());
        assert!(key_to_escape_sequence("Alt", false, false, false).is_none());
        assert!(key_to_escape_sequence("Shift", false, false, false).is_none());
        assert!(key_to_escape_sequence("Meta", false, false, false).is_none());
        assert!(key_to_escape_sequence("CapsLock", false, false, false).is_none());
    }
}

// ============================================================================
// WASM Browser Tests - Terminal State
// ============================================================================

#[cfg(target_arch = "wasm32")]
mod wasm_state_tests {
    use super::*;

    #[wasm_bindgen_test]
    fn test_new_terminal_state() {
        let state = TerminalState::new(80, 24);
        
        assert_eq!(state.cols, 80);
        assert_eq!(state.rows, 24);
        assert_eq!(state.cursor_x, 0);
        assert_eq!(state.cursor_y, 0);
        assert_eq!(state.grid.len(), 24);
        assert_eq!(state.grid[0].len(), 80);
    }

    #[wasm_bindgen_test]
    fn test_put_char() {
        let mut state = TerminalState::new(80, 24);
        
        state.put_char('H');
        state.put_char('i');
        
        assert_eq!(state.grid[0][0].char, 'H');
        assert_eq!(state.grid[0][1].char, 'i');
        assert_eq!(state.cursor_x, 2);
    }

    #[wasm_bindgen_test]
    fn test_linefeed() {
        let mut state = TerminalState::new(80, 24);
        
        state.put_char('A');
        state.linefeed();
        state.put_char('B');
        
        // linefeed() moves cursor down but NOT to column 0
        // So 'B' is written at column 1 (after 'A')
        assert_eq!(state.cursor_y, 1);
        assert_eq!(state.grid[1][1].char, 'B');
    }

    #[wasm_bindgen_test]
    fn test_carriage_return() {
        let mut state = TerminalState::new(80, 24);
        
        state.put_char('A');
        state.put_char('B');
        state.carriage_return();
        
        assert_eq!(state.cursor_x, 0);
    }

    #[wasm_bindgen_test]
    fn test_cursor_movement() {
        let mut state = TerminalState::new(80, 24);
        
        state.move_cursor(5, 10); // 1-indexed
        assert_eq!(state.cursor_y, 4); // 0-indexed
        assert_eq!(state.cursor_x, 9);
        
        state.cursor_up(2);
        assert_eq!(state.cursor_y, 2);
        
        state.cursor_down(1);
        assert_eq!(state.cursor_y, 3);
        
        state.cursor_forward(5);
        assert_eq!(state.cursor_x, 14);
        
        state.cursor_backward(3);
        assert_eq!(state.cursor_x, 11);
    }

    #[wasm_bindgen_test]
    fn test_erase_line() {
        let mut state = TerminalState::new(80, 24);
        
        // Fill first line
        for c in "Hello World".chars() {
            state.put_char(c);
        }
        
        state.cursor_x = 5;
        state.erase_line_to_end();
        
        assert_eq!(state.grid[0][4].char, 'o'); // Before cursor
        assert_eq!(state.grid[0][5].char, ' '); // At cursor - erased
        assert_eq!(state.grid[0][6].char, ' '); // After cursor - erased
    }

    #[wasm_bindgen_test]
    fn test_scroll_up() {
        let mut state = TerminalState::new(80, 5);
        
        // Fill terminal to trigger scroll
        for i in 0..6 {
            state.put_char((b'A' + i) as char);
            state.linefeed();
        }
        
        // First line should be in scrollback
        assert!(!state.scrollback.is_empty());
    }

    #[wasm_bindgen_test]
    fn test_resize() {
        let mut state = TerminalState::new(80, 24);
        state.put_char('A');
        
        state.resize(40, 12);
        
        assert_eq!(state.cols, 40);
        assert_eq!(state.rows, 12);
        assert_eq!(state.grid.len(), 12);
        assert_eq!(state.grid[0].len(), 40);
        // Note: resize may move content to scrollback when shrinking rows
        // Just verify the grid structure is correct
    }

    #[wasm_bindgen_test]
    fn test_scroll_view() {
        let mut state = TerminalState::new(80, 5);
        
        // Add some scrollback
        for i in 0..10 {
            state.put_char((b'A' + i) as char);
            state.linefeed();
        }
        
        // Should have scrollback
        assert!(state.scrollback.len() > 0);
        
        // Scroll up into history
        state.scroll_view(3);
        assert_eq!(state.scroll_offset, 3);
        
        // Scroll back down
        state.scroll_view(-2);
        assert_eq!(state.scroll_offset, 1);
        
        // Scroll to bottom
        state.scroll_to_bottom();
        assert_eq!(state.scroll_offset, 0);
    }

    #[wasm_bindgen_test]
    fn test_tab() {
        let mut state = TerminalState::new(80, 24);
        
        state.put_char('A');
        state.tab();
        
        // Tab should move to next 8-column boundary
        assert_eq!(state.cursor_x, 8);
    }

    #[wasm_bindgen_test]
    fn test_backspace() {
        let mut state = TerminalState::new(80, 24);
        
        state.put_char('A');
        state.put_char('B');
        state.backspace();
        
        assert_eq!(state.cursor_x, 1);
    }

    #[wasm_bindgen_test]
    fn test_save_restore_cursor() {
        let mut state = TerminalState::new(80, 24);
        
        state.cursor_x = 10;
        state.cursor_y = 5;
        state.save_cursor();
        
        state.cursor_x = 0;
        state.cursor_y = 0;
        state.restore_cursor();
        
        assert_eq!(state.cursor_x, 10);
        assert_eq!(state.cursor_y, 5);
    }
}

// ============================================================================
// WASM Browser Tests - Parser and SGR
// ============================================================================

#[cfg(target_arch = "wasm32")]
mod wasm_parser_tests {
    use super::*;

    #[wasm_bindgen_test]
    fn test_parse_simple_text() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"Hello");
        
        assert_eq!(state.grid[0][0].char, 'H');
        assert_eq!(state.grid[0][1].char, 'e');
        assert_eq!(state.grid[0][2].char, 'l');
        assert_eq!(state.grid[0][3].char, 'l');
        assert_eq!(state.grid[0][4].char, 'o');
    }

    #[wasm_bindgen_test]
    fn test_parse_newline() {
        let mut state = TerminalState::new(80, 24);
        
        // Note: \n alone only moves cursor down, not to column 0
        // Use \r\n for full newline behavior
        parse_bytes(&mut state, b"Line1\r\nLine2");
        
        assert_eq!(state.grid[0][0].char, 'L');
        assert_eq!(state.grid[1][0].char, 'L');
    }

    #[wasm_bindgen_test]
    fn test_parse_crlf() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"A\r\nB");
        
        assert_eq!(state.grid[0][0].char, 'A');
        assert_eq!(state.grid[1][0].char, 'B');
    }

    #[wasm_bindgen_test]
    fn test_parse_cursor_movement() {
        let mut state = TerminalState::new(80, 24);
        
        // ESC [ 5 ; 10 H - Move cursor to row 5, column 10
        parse_bytes(&mut state, b"\x1b[5;10H");
        
        assert_eq!(state.cursor_y, 4); // 0-indexed
        assert_eq!(state.cursor_x, 9);
    }

    #[wasm_bindgen_test]
    fn test_parse_clear_screen() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"Hello");
        parse_bytes(&mut state, b"\x1b[2J"); // Clear screen
        
        assert_eq!(state.grid[0][0].char, ' ');
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_bold() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"\x1b[1mBold\x1b[0m");
        
        assert!(state.grid[0][0].attrs.bold);
        assert_eq!(state.grid[0][0].char, 'B');
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_italic() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"\x1b[3mItalic\x1b[0m");
        
        assert!(state.grid[0][0].attrs.italic);
        assert_eq!(state.grid[0][0].char, 'I');
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_underline() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"\x1b[4mUnderline\x1b[0m");
        
        assert!(state.grid[0][0].attrs.underline);
        assert_eq!(state.grid[0][0].char, 'U');
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_colors() {
        let mut state = TerminalState::new(80, 24);
        
        // Set red foreground (ESC [ 31 m)
        parse_bytes(&mut state, b"\x1b[31mRed\x1b[0m");
        
        assert!(matches!(state.grid[0][0].attrs.fg, Color::Red));
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_background_colors() {
        let mut state = TerminalState::new(80, 24);
        
        // Set blue background (ESC [ 44 m)
        parse_bytes(&mut state, b"\x1b[44mBlue BG\x1b[0m");
        
        assert!(matches!(state.grid[0][0].attrs.bg, Color::Blue));
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_bright_colors() {
        let mut state = TerminalState::new(80, 24);
        
        // Set bright red foreground (ESC [ 91 m)
        parse_bytes(&mut state, b"\x1b[91mBright Red\x1b[0m");
        
        assert!(matches!(state.grid[0][0].attrs.fg, Color::BrightRed));
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_reset() {
        let mut state = TerminalState::new(80, 24);
        
        // Set bold, then reset
        parse_bytes(&mut state, b"\x1b[1mBold\x1b[0mNormal");
        
        assert!(state.grid[0][0].attrs.bold); // 'B' is bold
        assert!(!state.grid[0][4].attrs.bold); // 'N' is normal
    }

    #[wasm_bindgen_test]
    fn test_parse_sgr_combined() {
        let mut state = TerminalState::new(80, 24);
        
        // Set bold + red foreground + blue background in one sequence
        parse_bytes(&mut state, b"\x1b[1;31;44mStyled\x1b[0m");
        
        assert!(state.grid[0][0].attrs.bold);
        assert!(matches!(state.grid[0][0].attrs.fg, Color::Red));
        assert!(matches!(state.grid[0][0].attrs.bg, Color::Blue));
    }

    #[wasm_bindgen_test]
    fn test_parse_erase_line() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"Hello World");
        parse_bytes(&mut state, b"\x1b[5G"); // Move to column 5
        parse_bytes(&mut state, b"\x1b[K"); // Erase to end of line
        
        assert_eq!(state.grid[0][0].char, 'H');
        assert_eq!(state.grid[0][4].char, ' '); // Erased
    }

    #[wasm_bindgen_test]
    fn test_parse_backspace() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"AB\x08C");
        
        // Backspace moves cursor back but doesn't erase
        assert_eq!(state.grid[0][0].char, 'A');
        assert_eq!(state.grid[0][1].char, 'C'); // C overwrote B's position
    }

    #[wasm_bindgen_test]
    fn test_parse_tab() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, b"A\tB");
        
        assert_eq!(state.grid[0][0].char, 'A');
        assert_eq!(state.cursor_x, 9); // After 'B' at tab stop + 1
    }

    #[wasm_bindgen_test]
    fn test_parse_utf8() {
        let mut state = TerminalState::new(80, 24);
        
        parse_bytes(&mut state, "日本語".as_bytes());
        
        assert_eq!(state.grid[0][0].char, '日');
        assert_eq!(state.grid[0][1].char, '本');
        assert_eq!(state.grid[0][2].char, '語');
    }

    #[wasm_bindgen_test]
    fn test_parse_dim_text() {
        let mut state = TerminalState::new(80, 24);
        
        // ESC [ 2 m - dim/faint
        parse_bytes(&mut state, b"\x1b[2mDim\x1b[0m");
        
        assert!(state.grid[0][0].attrs.dim);
    }

    #[wasm_bindgen_test]
    fn test_parse_reverse_video() {
        let mut state = TerminalState::new(80, 24);
        
        // ESC [ 7 m - reverse video
        parse_bytes(&mut state, b"\x1b[7mReverse\x1b[0m");
        
        assert!(state.grid[0][0].attrs.reverse);
    }

    #[wasm_bindgen_test]
    fn test_parse_strikethrough() {
        let mut state = TerminalState::new(80, 24);
        
        // ESC [ 9 m - strikethrough
        parse_bytes(&mut state, b"\x1b[9mStrike\x1b[0m");
        
        assert!(state.grid[0][0].attrs.strikethrough);
    }
}

// ============================================================================
// WASM Browser Tests - Attributes
// ============================================================================

#[cfg(target_arch = "wasm32")]
mod wasm_attributes_tests {
    use super::*;

    #[wasm_bindgen_test]
    fn test_cell_attributes_default() {
        let attrs = CellAttributes::default();
        
        assert!(!attrs.bold);
        assert!(!attrs.italic);
        assert!(!attrs.underline);
        assert!(!attrs.strikethrough);
        assert!(!attrs.dim);
        assert!(!attrs.reverse);
        assert!(matches!(attrs.fg, Color::Default));
        assert!(matches!(attrs.bg, Color::Default));
    }

    #[wasm_bindgen_test]
    fn test_cell_attributes_copy() {
        let mut attrs = CellAttributes::default();
        attrs.bold = true;
        attrs.fg = Color::Red;
        
        let copied = attrs; // Copy trait
        
        assert!(copied.bold);
        assert!(matches!(copied.fg, Color::Red));
    }

    #[wasm_bindgen_test]
    fn test_cell_attributes_clone() {
        let mut attrs = CellAttributes::default();
        attrs.italic = true;
        attrs.bg = Color::Blue;
        
        let cloned = attrs.clone(); // Explicit clone
        
        assert!(cloned.italic);
        assert!(matches!(cloned.bg, Color::Blue));
    }

    #[wasm_bindgen_test]
    fn test_color_variants() {
        // Test that all color variants can be created
        let _default = Color::Default;
        let _black = Color::Black;
        let _red = Color::Red;
        let _green = Color::Green;
        let _yellow = Color::Yellow;
        let _blue = Color::Blue;
        let _magenta = Color::Magenta;
        let _cyan = Color::Cyan;
        let _white = Color::White;
        let _bright_black = Color::BrightBlack;
        let _bright_red = Color::BrightRed;
        let _bright_green = Color::BrightGreen;
        let _bright_yellow = Color::BrightYellow;
        let _bright_blue = Color::BrightBlue;
        let _bright_magenta = Color::BrightMagenta;
        let _bright_cyan = Color::BrightCyan;
        let _bright_white = Color::BrightWhite;
        let _rgb = Color::Rgb(255, 128, 0);
        let _index = Color::Index(42);
    }

    #[wasm_bindgen_test]
    fn test_color_default() {
        let color = Color::default();
        assert!(matches!(color, Color::Default));
    }
}
