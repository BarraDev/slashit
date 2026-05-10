use leptos::prelude::*;
use leptos::callback::Callback;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use std::rc::Rc;
use std::cell::RefCell;

use web_sys::js_sys;
use futures::future::{select, Either};

use super::input::key_to_escape_sequence;
use super::parser::parse_bytes;
use super::render::cell_style;
use super::state::{TerminalState, CellAttributes};
use crate::services::pty_service;

// Tauri invoke for commands
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = "listen")]
    fn tauri_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> js_sys::Promise;
}

// Character cell dimensions for monospace font (text-sm = 14px, line-height 1.2)
// These are approximate and will be used for initial sizing
const CHAR_WIDTH: f64 = 8.4;  // Approximate width for 14px monospace
const CHAR_HEIGHT: f64 = 16.8; // 14px * 1.2 line-height
const MIN_COLS: u16 = 40;
const MIN_ROWS: u16 = 10;
const MAX_COLS: u16 = 500;
const MAX_ROWS: u16 = 200;

/// Calculate terminal dimensions from pixel size
fn calculate_dimensions(width: f64, height: f64) -> (u16, u16) {
    // Account for padding (8px on each side = 16px total)
    let usable_width = (width - 16.0).max(0.0);
    let usable_height = (height - 16.0).max(0.0);
    
    let cols = (usable_width / CHAR_WIDTH) as u16;
    let rows = (usable_height / CHAR_HEIGHT) as u16;
    
    // Clamp to reasonable values
    let cols = cols.clamp(MIN_COLS, MAX_COLS);
    let rows = rows.clamp(MIN_ROWS, MAX_ROWS);
    
    (cols, rows)
}

/// A serializable representation of a terminal cell for the view
#[derive(Clone, Debug)]
struct ViewCell {
    char: char,
    attrs: CellAttributes,
}

/// A serializable representation of terminal state for the view
#[derive(Clone, Debug)]
struct ViewState {
    grid: Vec<Vec<ViewCell>>,
    cursor_x: usize,
    cursor_y: usize,
    scroll_offset: usize,
}

impl ViewState {
    fn from_terminal_state(state: &TerminalState) -> Self {
        // Get visible rows (accounting for scroll offset)
        let visible = state.visible_rows();
        Self {
            grid: visible.iter().map(|row| {
                row.iter().map(|cell| ViewCell {
                    char: cell.char,
                    attrs: cell.attrs,
                }).collect()
            }).collect(),
            // Only show cursor if not scrolled up
            cursor_x: if state.scroll_offset == 0 { state.cursor_x } else { usize::MAX },
            cursor_y: if state.scroll_offset == 0 { state.cursor_y } else { usize::MAX },
            scroll_offset: state.scroll_offset,
        }
    }
    
    fn empty(cols: usize, rows: usize) -> Self {
        Self {
            grid: (0..rows).map(|_| {
                (0..cols).map(|_| ViewCell {
                    char: ' ',
                    attrs: CellAttributes::default(),
                }).collect()
            }).collect(),
            cursor_x: 0,
            cursor_y: 0,
            scroll_offset: 0,
        }
    }

    /// Create a demo terminal state with a welcome message
    fn demo(cols: usize, rows: usize, name: &str) -> Self {
        let mut state = Self::empty(cols, rows);
        
        // Add welcome message
        let lines = [
            format!("Welcome to {} (Demo Mode)", name),
            String::new(),
            "PTY connection unavailable - running in demo mode.".to_string(),
            "Type commands below (simulated):".to_string(),
            String::new(),
            "$ ".to_string(),
        ];
        
        for (row_idx, line) in lines.iter().enumerate() {
            if row_idx >= rows {
                break;
            }
            for (col_idx, ch) in line.chars().enumerate() {
                if col_idx >= cols {
                    break;
                }
                state.grid[row_idx][col_idx].char = ch;
            }
        }
        
        state.cursor_x = 2;
        state.cursor_y = 5;
        state
    }
    
    /// Check if we're scrolled up (viewing history)
    fn is_scrolled(&self) -> bool {
        self.scroll_offset > 0
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum TerminalMode {
    Connecting,
    Connected,
    Reconnecting,  // Auto-reconnecting after connection loss
    Exited,  // Shell has exited
    Demo,
    Error,
}

/// Auto-reconnect configuration
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const RECONNECT_BASE_DELAY_MS: u32 = 1000;
const RECONNECT_MAX_DELAY_MS: u32 = 30000;

/// Exit information for displaying to user
#[derive(Clone, Debug, Default)]
struct ExitInfo {
    exit_code: Option<i32>,
    reason: String,
}

/// Reconnect state tracking
#[derive(Clone, Debug, Default)]
struct ReconnectState {
    attempt: u32,
}

struct ListenerCtx {
    session_id: String,
    cols: usize,
    rows: usize,
    set_view_state: WriteSignal<ViewState>,
    initial_scrollback: Option<Vec<u8>>,
    terminal_state_rc: Rc<RefCell<Option<TerminalState>>>,
    set_mode: Option<WriteSignal<TerminalMode>>,
    set_exit_info: Option<WriteSignal<ExitInfo>>,
    on_close: Option<Callback<()>>,
    reconnect_trigger: Option<RwSignal<bool>>,
}

/// Setup Tauri event listener for PTY output with shared state
fn setup_pty_listener_with_state(ctx: ListenerCtx) {
    let ListenerCtx {
        session_id, cols, rows, set_view_state, initial_scrollback,
        terminal_state_rc, set_mode, set_exit_info, on_close, reconnect_trigger,
    } = ctx;
    // Create terminal state and store it in the shared Rc
    let mut state = TerminalState::new(cols, rows);
    
    // If we have scrollback, apply it first
    if let Some(scrollback) = initial_scrollback {
        if !scrollback.is_empty() {
            leptos::logging::log!("[Terminal] Applying {} bytes of scrollback", scrollback.len());
            parse_bytes(&mut state, &scrollback);
            // Make sure we're at the bottom after loading scrollback
            state.scroll_to_bottom();
            set_view_state.set(ViewState::from_terminal_state(&state));
        }
    }
    
    // Store in shared Rc
    *terminal_state_rc.borrow_mut() = Some(state);
    
    let terminal_state_clone = terminal_state_rc.clone();
    let sid = session_id.clone();
    
    leptos::logging::log!("[Terminal] Setting up event listener for session: {} ({}x{})", session_id, cols, rows);
    
    // Create the event handler closure
    let handler = Closure::new(move |event: JsValue| {
        // Parse the event payload
        // Tauri events have structure: { event: string, windowLabel: string, payload: T }
        if let Ok(payload_obj) = js_sys::Reflect::get(&event, &JsValue::from_str("payload")) {
            // Check session_id matches
            if let Ok(session_id_val) = js_sys::Reflect::get(&payload_obj, &JsValue::from_str("session_id")) {
                if let Some(event_sid) = session_id_val.as_string() {
                    if event_sid != sid {
                        return;
                    }
                }
            }
            
            // Get the data array
            if let Ok(data_val) = js_sys::Reflect::get(&payload_obj, &JsValue::from_str("data")) {
                if let Some(array) = data_val.dyn_ref::<js_sys::Array>() {
                    let bytes: Vec<u8> = array
                        .iter()
                        .filter_map(|v| v.as_f64().map(|n| n as u8))
                        .collect();
                    
                    if !bytes.is_empty() {
                        // Process through VTE parser
                        let mut state_opt = terminal_state_clone.borrow_mut();
                        if let Some(ref mut state) = *state_opt {
                            parse_bytes(state, &bytes);
                            // Auto-scroll to bottom when new output arrives (except if user is actively scrolling)
                            // Small scroll offsets auto-scroll, larger ones don't
                            if state.scroll_offset <= 5 {
                                state.scroll_to_bottom();
                            }
                            // Update the view state signal
                            set_view_state.set(ViewState::from_terminal_state(state));
                        }
                    }
                }
            }
        }
    });
    
    // Call Tauri's listen function
    let promise = tauri_listen("pty-output", &handler);
    
    // Convert promise to future and spawn it
    let future = wasm_bindgen_futures::JsFuture::from(promise);
    spawn_local(async move {
        match future.await {
            Ok(_unlisten) => {
                leptos::logging::log!("[Terminal] Event listener registered successfully");
            }
            Err(e) => {
                leptos::logging::error!("[Terminal] Failed to register listener: {:?}", e);
            }
        }
    });
    
    // Keep the closure alive - it needs to persist for the lifetime of the terminal
    handler.forget();
    
    // Also listen for exit events if we have a set_mode callback
    if let Some(set_mode) = set_mode {
        let exit_sid = session_id.clone();
        let reconnect_trigger_for_exit = reconnect_trigger;
        let exit_handler = Closure::new(move |event: JsValue| {
            // Parse the event payload
            if let Ok(payload_obj) = js_sys::Reflect::get(&event, &JsValue::from_str("payload")) {
                // Check session_id matches
                if let Ok(session_id_val) = js_sys::Reflect::get(&payload_obj, &JsValue::from_str("session_id")) {
                    if let Some(event_sid) = session_id_val.as_string() {
                        if event_sid == exit_sid {
                            // Extract exit code
                            let exit_code = js_sys::Reflect::get(&payload_obj, &JsValue::from_str("exit_code"))
                                .ok()
                                .and_then(|v| v.as_f64())
                                .map(|n| n as i32);
                            
                            // Extract reason
                            let reason = js_sys::Reflect::get(&payload_obj, &JsValue::from_str("reason"))
                                .ok()
                                .and_then(|v| v.as_string())
                                .unwrap_or_else(|| "Shell exited".to_string());
                            
                            leptos::logging::log!("[Terminal] Shell exited for session: {}, exit_code: {:?}, reason: {}", exit_sid, exit_code, reason);
                            
                            // Check if this was an unexpected disconnection (not a clean exit)
                            // Only trigger auto-reconnect for connection errors, not for normal shell exits
                            // A missing exit code (None) indicates unexpected disconnect
                            let is_connection_error = exit_code.is_none()
                                || reason.to_lowercase().contains("error")
                                || reason.to_lowercase().contains("disconnect")
                                || reason.to_lowercase().contains("connection");
                            
                            if let Some(ref set_exit_info) = set_exit_info {
                                set_exit_info.set(ExitInfo {
                                    exit_code,
                                    reason: reason.clone(),
                                });
                            }
                            
                            // For connection errors, trigger auto-reconnect instead of showing exited state
                            if is_connection_error {
                                if let Some(trigger) = reconnect_trigger_for_exit {
                                    leptos::logging::log!("[Terminal] Error exit detected, triggering auto-reconnect");
                                    trigger.set(true);
                                    return;
                                }
                            }
                            
                            set_mode.set(TerminalMode::Exited);
                        }
                    }
                }
            }
        });
        
        let exit_promise = tauri_listen("pty-exit", &exit_handler);
        let exit_future = wasm_bindgen_futures::JsFuture::from(exit_promise);
        spawn_local(async move {
            match exit_future.await {
                Ok(_) => leptos::logging::log!("[Terminal] Exit event listener registered"),
                Err(e) => leptos::logging::error!("[Terminal] Failed to register exit listener: {:?}", e),
            }
        });
        exit_handler.forget();
    }
}

#[component]
pub fn RealTerminal(
    #[prop(into)] name: String,
    session_id: Option<String>,
    working_directory: Option<String>,
    on_session_created: Callback<Option<String>>,
    on_close: Callback<()>,
) -> impl IntoView {
    let (current_session_id, set_current_session_id) = signal(session_id.clone());
    let (error, set_error) = signal(None::<String>);
    let (mode, set_mode) = signal(TerminalMode::Connecting);
    let (exit_info, set_exit_info) = signal(ExitInfo::default());
    let (reconnect_state, set_reconnect_state) = signal(ReconnectState::default());
    let (is_focused, set_is_focused) = signal(false);
    let (input_buffer, set_input_buffer) = signal(String::new());
    
    // Signal to trigger auto-reconnect
    let reconnect_trigger = RwSignal::new(false);
    
    // Dynamic terminal dimensions - use RwSignal for both read and write access
    let cols = RwSignal::new(80u16);
    let rows = RwSignal::new(24u16);
    
    // Node reference for the terminal container to manage focus
    let terminal_ref = NodeRef::<leptos::html::Div>::new();
    // Node reference for the terminal content area (for ResizeObserver)
    let content_ref = NodeRef::<leptos::html::Div>::new();
    
    // Store terminal view state in a signal - use Rc<RefCell> for shared mutable state
    let terminal_state_rc: Rc<RefCell<Option<TerminalState>>> = Rc::new(RefCell::new(None));
    let (view_state, set_view_state) = signal(ViewState::empty(80, 24));
    
    // Track if we've done initial spawn - use RwSignal for both read and write
    let has_spawned = RwSignal::new(false);
    
    // Track if ResizeObserver has fired with valid dimensions
    let dimensions_ready = RwSignal::new(false);
    
    // Signal to trigger restart - when set to true, spawn a new session
    let restart_trigger = RwSignal::new(false);
    
    // Effect guards - ensure effects run exactly once (fixes Leptos 0.8 effect behavior)
    let spawn_effect_ran = RwSignal::new(false);
    let timeout_effect_ran = RwSignal::new(false);
    
    // Clone name for different uses
    let name_for_spawn = name.clone();
    let name_for_demo = name.clone();
    let name_display = name;
    
    // Clone working_directory for spawn
    let working_dir_for_spawn = working_directory.clone();
    let working_dir_for_restart = working_directory.clone();
    
    // Setup ResizeObserver to track container size changes
    // Use spawn_local with retries to ensure DOM is mounted before setting up observer
    let terminal_state_for_resize = terminal_state_rc.clone();
    Effect::new(move |_| {
        let terminal_state_for_callback = terminal_state_for_resize.clone();
        
        spawn_local(async move {
            // Try multiple times with increasing delays to get the content_ref
            // This handles the case where the DOM isn't mounted yet when the effect runs
            let mut content_el_option = None;
            for attempt in 0..10 {
                if let Some(el) = content_ref.get() {
                    content_el_option = Some(el);
                    break;
                }
                // Increasing delays: 10ms, 50ms, 100ms, 150ms...
                let delay = if attempt == 0 { 10 } else { 50 * attempt as u32 };
                leptos::logging::log!("[Terminal] ResizeObserver: content_ref not ready (attempt {}), waiting {}ms", attempt + 1, delay);
                gloo_timers::future::TimeoutFuture::new(delay).await;
            }
            
            let Some(content_el) = content_el_option else {
                leptos::logging::error!("[Terminal] ResizeObserver: content_ref never became available after 10 attempts!");
                // Set fallback dimensions so spawn can proceed
                leptos::logging::warn!("[Terminal] Using fallback dimensions 80x24");
                cols.set(80);
                rows.set(24);
                dimensions_ready.set(true);
                return;
            };
            
            let content_el: &web_sys::Element = &content_el;
            
            leptos::logging::log!("[Terminal] Setting up ResizeObserver on content element");
        
            // Create ResizeObserver callback
            let cols_signal = cols;
            let rows_signal = rows;
            let set_view_state_clone = set_view_state;
            let current_session_id_clone = current_session_id;
            let mode_clone = mode;
            let terminal_state_for_observer = terminal_state_for_callback.clone();
        
            let callback: Closure<dyn Fn(js_sys::Array)> = Closure::new(move |entries: js_sys::Array| {
                if let Some(entry) = entries.get(0).dyn_ref::<web_sys::ResizeObserverEntry>() {
                    let content_rect = entry.content_rect();
                    let width = content_rect.width();
                    let height = content_rect.height();
                    
                    if width > 0.0 && height > 0.0 {
                        let (new_cols, new_rows) = calculate_dimensions(width, height);
                        let old_cols = cols_signal.get_untracked();
                        let old_rows = rows_signal.get_untracked();
                        
                        // Only update if dimensions actually changed
                        if new_cols != old_cols || new_rows != old_rows {
                            leptos::logging::log!("[Terminal] Resize detected: {}x{} -> {}x{} (container: {}x{}px)", 
                                old_cols, old_rows, new_cols, new_rows, width as u32, height as u32);
                            
                            cols_signal.set(new_cols);
                            rows_signal.set(new_rows);
                            
                            // Mark dimensions as ready (first resize observer callback)
                            dimensions_ready.set(true);
                            
                            // Resize the PTY if connected
                            if mode_clone.get_untracked() == TerminalMode::Connected {
                                if let Some(sid) = current_session_id_clone.get_untracked() {
                                    let sid_for_resize = sid.clone();
                                    spawn_local(async move {
                                        if let Err(e) = pty_service::resize_pty(sid_for_resize, new_cols, new_rows).await {
                                            leptos::logging::warn!("[Terminal] Failed to resize PTY: {}", e);
                                        }
                                    });
                                }
                            }
                            
                            // Update terminal state grid if we have state
                            let mut state_opt = terminal_state_for_observer.borrow_mut();
                            if let Some(ref mut state) = *state_opt {
                                state.resize(new_cols as usize, new_rows as usize);
                                set_view_state_clone.set(ViewState::from_terminal_state(state));
                            } else {
                                // No state yet, just update the empty view
                                set_view_state_clone.set(ViewState::empty(new_cols as usize, new_rows as usize));
                            }
                        }
                    }
                }
            });
            
            // Create and start the ResizeObserver
            let observer = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref())
                .expect("Failed to create ResizeObserver");
            observer.observe(content_el);
            
            // Keep the closure alive
            callback.forget();
            
            // Note: We're not cleaning up the observer on unmount for simplicity
            // In a production app, you'd want to disconnect it
        });
    });
    
    // Spawn PTY after we have initial dimensions from resize observer
    let existing_session_id = session_id.clone();
    let terminal_state_for_spawn = terminal_state_rc.clone();
    let working_dir_for_initial_spawn = working_dir_for_spawn.clone();
    Effect::new(move |_| {
        // Use explicit signal guard instead of prev pattern for reliable one-time execution
        if spawn_effect_ran.get_untracked() {
            return;
        }
        spawn_effect_ran.set(true);
        
        leptos::logging::log!("[Terminal] Spawn effect triggered - initiating PTY spawn sequence");
        
        // Wait a tick for ResizeObserver to fire and set dimensions
        let name = name_for_spawn.clone();
        let existing_id = existing_session_id.clone();
        let terminal_state_rc = terminal_state_for_spawn.clone();
        let wd_for_spawn = working_dir_for_initial_spawn.clone();
        
        // Wait for ResizeObserver to fire and give us real dimensions
        spawn_local(async move {
            leptos::logging::log!("[Terminal] spawn_local async task started - waiting for dimensions");
            
            // Poll until dimensions are ready (ResizeObserver has fired)
            // with a maximum wait time of 2000ms (increased for slower systems/WebViews)
            let mut waited = 0u32;
            while !dimensions_ready.get_untracked() && waited < 2000 {
                gloo_timers::future::TimeoutFuture::new(50).await;
                waited += 50;
                // Log progress every 500ms
                if waited.is_multiple_of(500) {
                    leptos::logging::log!("[Terminal] Still waiting for dimensions... ({}ms elapsed)", waited);
                }
            }
            
            // If dimensions still not ready after timeout, set defaults and proceed anyway
            if !dimensions_ready.get_untracked() {
                leptos::logging::warn!("[Terminal] ResizeObserver didn't fire in time (waited {}ms), using default dimensions 80x24", waited);
                cols.set(80);
                rows.set(24);
                dimensions_ready.set(true);  // Mark as ready with defaults
            } else {
                leptos::logging::log!("[Terminal] Dimensions ready after {}ms: {}x{}", waited, cols.get_untracked(), rows.get_untracked());
            }
            
            // Check if we already spawned (in case effect runs twice)
            if has_spawned.get_untracked() {
                leptos::logging::warn!("[Terminal] PTY already spawned, skipping duplicate spawn");
                return;
            }
            has_spawned.set(true);
            
            let current_cols = cols.get_untracked();
            let current_rows = rows.get_untracked();
            
            leptos::logging::log!("[Terminal] Proceeding with PTY spawn: {}x{}", current_cols, current_rows);
            
            if let Some(sid) = existing_id {
                // Attach to existing session
                leptos::logging::log!("[Terminal] Attaching to existing session: {}", sid);
                
                // First get scrollback
                let scrollback = match pty_service::get_pty_scrollback(sid.clone()).await {
                    Ok(data) => {
                        leptos::logging::log!("[Terminal] Got {} bytes of scrollback", data.len());
                        Some(data)
                    }
                    Err(e) => {
                        leptos::logging::warn!("[Terminal] Failed to get scrollback: {}", e);
                        None
                    }
                };
                
                // Attach to session
                match pty_service::attach_pty_session(sid.clone()).await {
                    Ok(_info) => {
                        leptos::logging::log!("[Terminal] ✓ Attached to session: {}", sid);
                        set_current_session_id.set(Some(sid.clone()));
                        set_mode.set(TerminalMode::Connected);
                        
                        // Setup listener with scrollback
                        setup_pty_listener_with_state(ListenerCtx {
                            session_id: sid,
                            cols: current_cols as usize,
                            rows: current_rows as usize,
                            set_view_state,
                            initial_scrollback: scrollback,
                            terminal_state_rc: terminal_state_rc.clone(),
                            set_mode: Some(set_mode),
                            set_exit_info: Some(set_exit_info),
                            on_close: Some(on_close),
                            reconnect_trigger: Some(reconnect_trigger),
                        });
                        
                        // Resize to current dimensions
                        let sid_for_resize = current_session_id.get_untracked().unwrap();
                        if let Err(e) = pty_service::resize_pty(sid_for_resize, current_cols, current_rows).await {
                            leptos::logging::warn!("[Terminal] Failed to resize after attach: {}", e);
                        }
                    }
                    Err(e) => {
                        leptos::logging::error!("[Terminal] ✗ Failed to attach: {}, spawning new session", e);
                        // Session might have died, spawn new one
                        spawn_new_session_with_state(SpawnCtx { name, cols: current_cols, rows: current_rows, set_current_session_id, set_mode, set_error, set_view_state, on_session_created, terminal_state_rc: terminal_state_rc.clone(), cols_signal: cols, rows_signal: rows, set_exit_info, on_close, working_directory: wd_for_spawn.clone(), reconnect_trigger }).await;
                    }
                }
            } else {
                // Spawn new session
                leptos::logging::log!("[Terminal] Spawning new PTY session...");
                spawn_new_session_with_state(SpawnCtx { name, cols: current_cols, rows: current_rows, set_current_session_id, set_mode, set_error, set_view_state, on_session_created, terminal_state_rc: terminal_state_rc.clone(), cols_signal: cols, rows_signal: rows, set_exit_info, on_close, working_directory: wd_for_spawn.clone(), reconnect_trigger }).await;
            }
        });
    });

    // Auto-reconnect effect - handles reconnection when triggered
    let name_for_reconnect = name_display.clone();
    let terminal_state_for_reconnect = terminal_state_rc.clone();
    let on_session_created_for_reconnect = on_session_created;
    let on_close_for_reconnect = on_close;
    let working_dir_for_reconnect = working_dir_for_restart.clone();
    
    Effect::new(move |prev: Option<bool>| {
        let should_reconnect = reconnect_trigger.get();
        
        // Only act if we just triggered a reconnect (transition from false to true)
        if should_reconnect && prev == Some(false) {
            let current_attempt = reconnect_state.get_untracked().attempt + 1;
            
            if current_attempt > MAX_RECONNECT_ATTEMPTS {
                leptos::logging::error!("[Terminal] Max reconnect attempts ({}) exceeded", MAX_RECONNECT_ATTEMPTS);
                set_error.set(Some(format!("Connection lost. Max reconnect attempts ({}) exceeded.", MAX_RECONNECT_ATTEMPTS)));
                set_mode.set(TerminalMode::Error);
                reconnect_trigger.set(false);
                return should_reconnect;
            }
            
            let name = name_for_reconnect.clone();
            let current_cols = cols.get();
            let current_rows = rows.get();
            let terminal_state_rc = terminal_state_for_reconnect.clone();
            let on_session_created = on_session_created_for_reconnect;
            let on_close = on_close_for_reconnect;
            let wd = working_dir_for_reconnect.clone();
            
            // Update reconnect state
            set_reconnect_state.set(ReconnectState {
                attempt: current_attempt,
            });
            set_mode.set(TerminalMode::Reconnecting);
            
            // Kill old session if exists
            let old_session_id = current_session_id.get();
            
            spawn_local(async move {
                // Clean up old session
                if let Some(sid) = old_session_id {
                    let _ = pty_service::kill_pty(sid).await;
                }
                
                // Calculate backoff delay: 1s, 2s, 4s, 8s, 16s (capped at 30s)
                let delay_ms = (RECONNECT_BASE_DELAY_MS * (1 << (current_attempt - 1))).min(RECONNECT_MAX_DELAY_MS);
                leptos::logging::log!("[Terminal] Reconnect attempt {} of {} in {}ms", current_attempt, MAX_RECONNECT_ATTEMPTS, delay_ms);
                gloo_timers::future::TimeoutFuture::new(delay_ms).await;
                
                // Clear old terminal state
                *terminal_state_rc.borrow_mut() = None;
                set_view_state.set(ViewState::empty(current_cols as usize, current_rows as usize));
                
                // Attempt to spawn new session
                spawn_new_session_with_state(SpawnCtx {
                    name,
                    cols: current_cols,
                    rows: current_rows,
                    set_current_session_id,
                    set_mode,
                    set_error,
                    set_view_state,
                    on_session_created,
                    terminal_state_rc,
                    cols_signal: cols,
                    rows_signal: rows,
                    set_exit_info,
                    on_close,
                    working_directory: wd,
                    reconnect_trigger,
                }).await;
                
                // If we successfully connected, reset reconnect state
                if mode.get_untracked() == TerminalMode::Connected {
                    set_reconnect_state.set(ReconnectState::default());
                }
                
                // Reset trigger
                reconnect_trigger.set(false);
            });
        }
        
        should_reconnect
    });

    // Clone signals for restart effect
    let name_for_restart_effect = name_display.clone();
    let terminal_state_for_restart = terminal_state_rc.clone();
    let on_session_created_for_restart = on_session_created;
    let on_close_for_restart = on_close;
    let working_dir_for_restart_effect = working_dir_for_restart.clone();
    
    // Connection timeout effect - switch to Error mode if stuck in Connecting for too long
    Effect::new(move |_| {
        // Use explicit signal guard instead of prev pattern for reliable one-time execution
        if timeout_effect_ran.get_untracked() {
            return;
        }
        timeout_effect_ran.set(true);
        
        leptos::logging::log!("[Terminal] Connection timeout effect started (10s timeout)");
        
        spawn_local(async move {
            // Wait for up to 10 seconds for connection
            gloo_timers::future::TimeoutFuture::new(10_000).await;
            
            let current_mode = mode.get_untracked();
            leptos::logging::log!("[Terminal] Timeout check - current mode: {:?}", current_mode);
            
            // If still connecting after timeout, switch to error mode
            if current_mode == TerminalMode::Connecting {
                leptos::logging::error!("[Terminal] Connection timeout after 10 seconds - PTY failed to start");
                set_error.set(Some("Connection timeout - PTY failed to start. Check browser console for details.".to_string()));
                set_mode.set(TerminalMode::Error);
            }
        });
    });

    // Effect to handle restart trigger
    Effect::new(move |prev: Option<bool>| {
        let should_restart = restart_trigger.get();
        
        // Only act if we just triggered a restart (transition from false to true)
        if should_restart && prev == Some(false) {
            let name = name_for_restart_effect.clone();
            let current_cols = cols.get();
            let current_rows = rows.get();
            let terminal_state_rc = terminal_state_for_restart.clone();
            let on_session_created = on_session_created_for_restart;
            let wd_for_restart = working_dir_for_restart_effect.clone();
            
            // Reset state
            set_mode.set(TerminalMode::Connecting);
            set_error.set(None);
            set_view_state.set(ViewState::empty(current_cols as usize, current_rows as usize));
            *terminal_state_rc.borrow_mut() = None;
            
            // Kill old session and spawn new one
            let old_session_id = current_session_id.get();
            spawn_local(async move {
                // Clean up old dead session
                if let Some(sid) = old_session_id {
                    let _ = pty_service::kill_pty(sid).await;
                }
                
                spawn_new_session_with_state(SpawnCtx {
                    name,
                    cols: current_cols,
                    rows: current_rows,
                    set_current_session_id,
                    set_mode,
                    set_error,
                    set_view_state,
                    on_session_created,
                    terminal_state_rc,
                    cols_signal: cols,
                    rows_signal: rows,
                    set_exit_info,
                    on_close: on_close_for_restart,
                    working_directory: wd_for_restart,
                    reconnect_trigger,
                }).await;
                
                // Reset trigger and reconnect state after spawn completes
                restart_trigger.set(false);
                set_reconnect_state.set(ReconnectState::default());
            });
        }
        
        should_restart
    });
    
    // Clone terminal_state_rc for wheel handler before it's moved into keydown closure
    let terminal_state_for_wheel = terminal_state_rc.clone();
    
    // Handle keyboard input
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let current_mode = mode.get();
        
        // Always prevent default and stop propagation in terminal to prevent navigation
        // This ensures keyboard shortcuts don't trigger when terminal is focused
        if current_mode == TerminalMode::Exited || current_mode == TerminalMode::Error {
            ev.prevent_default();
            ev.stop_propagation();
            return;
        }
        
        match current_mode {
            TerminalMode::Connected => {
                // Real PTY mode - send to backend
                if let Some(sid) = current_session_id.get() {
                    let key = ev.key();
                    let ctrl = ev.ctrl_key();
                    let alt = ev.alt_key();
                    let shift = ev.shift_key();
                    
                    if let Some(bytes) = key_to_escape_sequence(&key, ctrl, alt, shift) {
                        ev.prevent_default();
                        ev.stop_propagation();
                        
                        // If scrolled up, scroll to bottom on any key press
                        {
                            let mut state_opt = terminal_state_rc.borrow_mut();
                            if let Some(ref mut state) = *state_opt {
                                if state.scroll_offset > 0 {
                                    state.scroll_to_bottom();
                                    set_view_state.set(ViewState::from_terminal_state(state));
                                }
                            }
                        }
                        
                        spawn_local(async move {
                            if let Err(e) = pty_service::write_pty(sid, bytes).await {
                                leptos::logging::warn!("[Terminal] Write error: {}", e);
                            }
                        });
                    }
                }
            }
            TerminalMode::Demo => {
                // Demo mode - simulate terminal locally
                let key = ev.key();
                ev.prevent_default();
                ev.stop_propagation();
                
                let current_cols = cols.get() as usize;
                let current_rows = rows.get() as usize;
                let demo_name = name_for_demo.clone();
                
                if key == "Enter" {
                    let cmd = input_buffer.get();
                    set_input_buffer.set(String::new());
                    
                    set_view_state.update(|state| {
                        state.cursor_y += 1;
                        state.cursor_x = 0;
                        
                        let response = match cmd.trim() {
                            "ls" => "file1.txt  file2.txt  src/  Cargo.toml",
                            "pwd" => "/home/user/project",
                            "whoami" => "user",
                            "date" => "Sat Feb  1 2025 12:00:00",
                            "help" => "Available commands: ls, pwd, whoami, date, clear, help",
                            "clear" => {
                                *state = ViewState::demo(current_cols, current_rows, &demo_name);
                                return;
                            }
                            "" => "",
                            _ => "command not found (demo mode)",
                        };
                        
                        if !response.is_empty() {
                            for (i, ch) in response.chars().enumerate() {
                                if state.cursor_x + i < current_cols && state.cursor_y < current_rows {
                                    state.grid[state.cursor_y][state.cursor_x + i].char = ch;
                                }
                            }
                            state.cursor_y += 1;
                        }
                        
                        if state.cursor_y < current_rows {
                            state.grid[state.cursor_y][0].char = '$';
                            state.grid[state.cursor_y][1].char = ' ';
                            state.cursor_x = 2;
                        }
                    });
                } else if key == "Backspace" {
                    set_input_buffer.update(|buf| {
                        buf.pop();
                    });
                    set_view_state.update(|state| {
                        if state.cursor_x > 2 {
                            state.cursor_x -= 1;
                            state.grid[state.cursor_y][state.cursor_x].char = ' ';
                        }
                    });
                } else if key.len() == 1 {
                    let ch = key.chars().next().unwrap();
                    set_input_buffer.update(|buf| buf.push(ch));
                    set_view_state.update(|state| {
                        if state.cursor_x < current_cols - 1 && state.cursor_y < current_rows {
                            state.grid[state.cursor_y][state.cursor_x].char = ch;
                            state.cursor_x += 1;
                        }
                    });
                }
            }
            _ => {}
        }
    };
    
    // Handle focus events
    let on_focus = move |_| {
        set_is_focused.set(true);
    };
    
    let on_blur = move |_| {
        set_is_focused.set(false);
    };
    
    // Handle click to focus
    let on_click = move |_| {
        if let Some(el) = terminal_ref.get() {
            let _ = el.focus();
        }
    };
    
    // Handle mouse wheel for scrolling
    let on_wheel = move |ev: web_sys::WheelEvent| {
        ev.prevent_default();
        
        // Only scroll if connected or exited (has content)
        let current_mode = mode.get();
        if current_mode != TerminalMode::Connected && current_mode != TerminalMode::Exited {
            return;
        }
        
        let delta_y = ev.delta_y();
        
        // Calculate scroll amount (3 lines per scroll step)
        // INVERT the direction: positive deltaY (scroll down) = show newer content = decrease offset
        // negative deltaY (scroll up) = show older content = increase offset
        let scroll_lines = if delta_y.abs() > 100.0 {
            // Likely pixel scrolling (trackpad) - invert sign
            (-delta_y / 50.0) as i32
        } else {
            // Likely line scrolling (mouse wheel) - invert sign
            (-delta_y.signum() * 3.0) as i32
        };
        
        if scroll_lines != 0 {
            let mut state_opt = terminal_state_for_wheel.borrow_mut();
            if let Some(ref mut state) = *state_opt {
                state.scroll_view(scroll_lines);
                set_view_state.set(ViewState::from_terminal_state(state));
            }
        }
    };

    // Handle close button - wrap in Rc for cloning
    let on_close_click = {
        Rc::new(move |_: web_sys::MouseEvent| {
            if let Some(sid) = current_session_id.get() {
                spawn_local(async move {
                    let _ = pty_service::kill_pty(sid).await;
                });
            }
            on_close.run(());
        })
    };
    let on_close_click_header = on_close_click.clone();

    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden h-full flex flex-col">
            // Header
            <div class="flex items-center justify-between px-3 py-2 bg-[#1a1a1a] border-b border-white/5 flex-shrink-0">
                <div class="flex items-center gap-2">
                    <div class=move || {
                        let color = match mode.get() {
                            TerminalMode::Connected => "bg-green-500",
                            TerminalMode::Reconnecting => "bg-orange-500 animate-pulse",
                            TerminalMode::Exited => {
                                // Green for clean exit, red for error exits
                                let exit = exit_info.get();
                                if exit.exit_code == Some(0) {
                                    "bg-green-500/50" // Dimmed green for clean exit
                                } else {
                                    "bg-red-500"
                                }
                            }
                            TerminalMode::Demo => "bg-blue-500",
                            TerminalMode::Error => "bg-red-500",
                            TerminalMode::Connecting => "bg-yellow-500 animate-pulse",
                        };
                        format!("w-2 h-2 rounded-full {}", color)
                    } />
                    <span class="font-medium text-white/90 text-sm">{name_display}</span>                        <span class="text-xs text-white/40">
                        {move || match mode.get() {
                            TerminalMode::Connected => format!("{}x{}", cols.get(), rows.get()),
                            TerminalMode::Exited => {
                                let exit = exit_info.get();
                                match exit.exit_code {
                                    Some(0) => "exited (0)".to_string(),
                                    Some(code) => format!("exited ({})", code),
                                    None => "exited".to_string(),
                                }
                            }
                            TerminalMode::Demo => "demo".to_string(),
                            TerminalMode::Error => "error".to_string(),
                            TerminalMode::Connecting => "connecting...".to_string(),
                            TerminalMode::Reconnecting => {
                                let state = reconnect_state.get();
                                format!("reconnecting ({}/{})", state.attempt, MAX_RECONNECT_ATTEMPTS)
                            }
                        }}
                    </span>
                </div>
                <button
                    on:click=move |ev| on_close_click_header(ev)
                    class="text-white/40 hover:text-white/60 transition-colors p-1 hover:bg-white/10 rounded"
                >
                    <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                    </svg>
                </button>
            </div>
            
            // Terminal content
            <div
                node_ref=content_ref
                class="flex-1 overflow-hidden"
                style="min-height: 200px;"
            >
            <div
                node_ref=terminal_ref
                tabindex="0"
                on:keydown=on_keydown
                on:focus=on_focus
                on:blur=on_blur
                on:click=on_click
                on:wheel=on_wheel
                class=move || format!(
                    "h-full w-full p-2 bg-black overflow-hidden font-mono text-sm cursor-text focus:outline-none select-none {}",
                    if is_focused.get() { "ring-2 ring-yellow-500/50 ring-inset" } else { "" }
                )
                style="line-height: 1.2;"
            >
                {move || {
                    match mode.get() {
                        TerminalMode::Connecting => {
                            view! {
                                <div class="flex items-center justify-center h-full">
                                    <div class="flex flex-col items-center gap-2">
                                        <svg class="w-6 h-6 animate-spin text-yellow-500" fill="none" viewBox="0 0 24 24">
                                            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                                            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                        </svg>
                                        <span class="text-white/40 text-xs">"Connecting to PTY..."</span>
                                        <span class="text-white/20 text-xs">"Waiting for Tauri IPC..."</span>
                                    </div>
                                </div>
                            }.into_any()
                        }
                        TerminalMode::Reconnecting => {
                            let state = reconnect_state.get();
                            view! {
                                <div class="flex items-center justify-center h-full">
                                    <div class="flex flex-col items-center gap-3">
                                        <svg class="w-8 h-8 animate-spin text-orange-500" fill="none" viewBox="0 0 24 24">
                                            <circle class="opacity-25" cx="12" cy="12" r="10" stroke="currentColor" stroke-width="4"></circle>
                                            <path class="opacity-75" fill="currentColor" d="M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4zm2 5.291A7.962 7.962 0 014 12H0c0 3.042 1.135 5.824 3 7.938l3-2.647z"></path>
                                        </svg>
                                        <div class="text-center">
                                            <div class="text-orange-400 font-medium">"Reconnecting..."</div>
                                            <div class="text-white/40 text-xs mt-1">
                                                {format!("Attempt {} of {}", state.attempt, MAX_RECONNECT_ATTEMPTS)}
                                            </div>
                                        </div>
                                        <button
                                            on:click=move |_| {
                                                set_reconnect_state.set(ReconnectState { attempt: MAX_RECONNECT_ATTEMPTS + 1 });
                                                set_error.set(Some("Reconnection cancelled by user".to_string()));
                                                set_mode.set(TerminalMode::Error);
                                            }
                                            class="px-3 py-1 rounded bg-white/10 hover:bg-white/20 text-white/60 text-xs transition-colors"
                                        >
                                            "Cancel"
                                        </button>
                                    </div>
                                </div>
                            }.into_any()
                        }
                        TerminalMode::Error => {
                            let err = error.get().unwrap_or_default();
                            view! {
                                <div class="flex items-center justify-center h-full">
                                    <div class="text-center">
                                        <div class="text-red-400 mb-2">"Connection Error"</div>
                                        <div class="text-white/40 text-xs max-w-[200px]">{err}</div>
                                    </div>
                                </div>
                            }.into_any()
                        }
                        TerminalMode::Exited => {
                            // Show the last terminal state with a bottom banner
                            let state = view_state.get();
                            let exit = exit_info.get();
                            let has_error = exit.exit_code.map(|c| c != 0).unwrap_or(true);
                            let is_scrolled = state.is_scrolled();
                            view! {
                                <div class="h-full w-full overflow-hidden relative flex flex-col">
                                    // Scroll indicator when scrolled up
                                    <Show when=move || is_scrolled>
                                        <div class="absolute top-0 left-0 right-0 bg-gradient-to-b from-yellow-500/20 to-transparent h-8 pointer-events-none z-10 flex items-start justify-center pt-1">
                                            <span class="text-xs text-yellow-500/80 bg-black/50 px-2 py-0.5 rounded">
                                                {format!("↑ {} lines", state.scroll_offset)}
                                            </span>
                                        </div>
                                    </Show>
                                    
                                    // Render the terminal grid (full opacity - user can still read it)
                                    <div class="flex-1 overflow-hidden">
                                        {state.grid.iter().map(|row| {
                                            view! {
                                                <div class="flex whitespace-pre" style="height: 1.2em;">
                                                    {row.iter().map(|cell| {
                                                        let style = cell_style(&cell.attrs);
                                                        let char_display = if cell.char == ' ' || cell.char == '\0' {
                                                            '\u{00A0}'
                                                        } else {
                                                            cell.char
                                                        };
                                                        view! {
                                                            <span style=style>{char_display.to_string()}</span>
                                                        }
                                                    }).collect::<Vec<_>>()}
                                                </div>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </div>
                                    
                                    // Bottom banner with exit info and actions (doesn't cover terminal output)
                                    <div class=move || format!(
                                        "flex-shrink-0 px-3 py-2 border-t flex items-center justify-between gap-4 {}",
                                        if has_error { "bg-red-500/10 border-red-500/30" } else { "bg-white/5 border-white/10" }
                                    )>
                                        // Exit status
                                        <div class="flex items-center gap-2">
                                            {if has_error {
                                                view! {
                                                    <svg class="w-4 h-4 text-red-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
                                                    </svg>
                                                }.into_any()
                                            } else {
                                                view! {
                                                    <svg class="w-4 h-4 text-green-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
                                                    </svg>
                                                }.into_any()
                                            }}
                                            <span class=move || format!(
                                                "text-sm font-medium {}",
                                                if has_error { "text-red-400" } else { "text-white/70" }
                                            )>
                                                "Shell exited"
                                            </span>
                                            <span class=move || format!(
                                                "text-xs font-mono px-1.5 py-0.5 rounded {}",
                                                if has_error { "bg-red-500/20 text-red-300" } else { "bg-green-500/20 text-green-300" }
                                            )>
                                                {move || format!("code {}", exit.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".to_string()))}
                                            </span>
                                            // Show reason if it's not the default
                                            {(!exit.reason.is_empty() && exit.reason != "Shell exited" && exit.reason != "Shell exited normally").then(|| view! {
                                                <span class="text-xs text-white/40">
                                                    {format!("- {}", exit.reason.clone())}
                                                </span>
                                            })}
                                        </div>
                                        
                                        // Action buttons
                                        <div class="flex gap-2">
                                            <button
                                                on:click=move |_| restart_trigger.set(true)
                                                class="px-3 py-1 rounded bg-yellow-500 hover:bg-yellow-600 text-black text-xs font-medium transition-colors flex items-center gap-1"
                                            >
                                                <svg class="w-3 h-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                                </svg>
                                                "Restart"
                                            </button>
                                            <button
                                                on:click=move |_| {
                                                    // Kill session and close
                                                    if let Some(sid) = current_session_id.get() {
                                                        spawn_local(async move {
                                                            let _ = pty_service::kill_pty(sid).await;
                                                        });
                                                    }
                                                    on_close.run(());
                                                }
                                                class="px-3 py-1 rounded bg-white/10 hover:bg-white/20 text-white/60 text-xs transition-colors"
                                            >
                                                "Close"
                                            </button>
                                        </div>
                                    </div>
                                </div>
                            }.into_any()
                        }
                        TerminalMode::Connected | TerminalMode::Demo => {
                            let state = view_state.get();
                            let focused = is_focused.get();
                            let is_scrolled = state.is_scrolled();
                            
                            view! {
                                <div class="h-full w-full overflow-hidden relative">
                                    // Scroll indicator
                                    <Show when=move || is_scrolled>
                                        <div class="absolute top-0 left-0 right-0 bg-gradient-to-b from-yellow-500/20 to-transparent h-8 pointer-events-none z-10 flex items-start justify-center pt-1">
                                            <span class="text-xs text-yellow-500/80 bg-black/50 px-2 py-0.5 rounded">
                                                {format!("↑ {} lines", state.scroll_offset)}
                                            </span>
                                        </div>
                                    </Show>
                                    {state.grid.iter().enumerate().map(|(row_idx, row)| {
                                        // Only show cursor if we're not scrolled up
                                        let cursor_in_row = !is_scrolled && state.cursor_y == row_idx;
                                        let cursor_x = state.cursor_x;
                                        
                                        view! {
                                            <div class="flex whitespace-pre" style="height: 1.2em;">
                                                {row.iter().enumerate().map(|(col_idx, cell)| {
                                                    let is_cursor = cursor_in_row && col_idx == cursor_x;
                                                    let style = cell_style(&cell.attrs);
                                                    let char_display = if cell.char == ' ' || cell.char == '\0' {
                                                        '\u{00A0}'
                                                    } else {
                                                        cell.char
                                                    };
                                                    
                                                    let cursor_class = if is_cursor {
                                                        if focused {
                                                            "bg-white text-black term-cursor"
                                                        } else {
                                                            "bg-white/30 text-white/70"
                                                        }
                                                    } else {
                                                        ""
                                                    };
                                                    
                                                    view! {
                                                        <span
                                                            class=cursor_class
                                                            style=style
                                                        >
                                                            {char_display.to_string()}
                                                        </span>
                                                    }
                                                }).collect::<Vec<_>>()}
                                            </div>
                                        }
                                    }).collect::<Vec<_>>()}
                                    
                                    // Click to focus hint
                                    <Show when=move || !is_focused.get()>
                                        <div class="absolute bottom-2 left-0 right-0 text-xs text-white/20 text-center pointer-events-none">
                                            "Click to focus • Scroll with mouse wheel"
                                        </div>
                                    </Show>
                                    
                                    // Scroll to bottom hint when scrolled up
                                    <Show when=move || is_scrolled>
                                        <div class="absolute bottom-2 left-0 right-0 text-xs text-yellow-500/60 text-center pointer-events-none">
                                            "Scroll down or press any key to return to bottom"
                                        </div>
                                    </Show>
                                </div>
                            }.into_any()
                        }
                    }
                }}
            </div>
            </div>
        </div>
    }
}

/// Maximum number of retry attempts for PTY spawn
const MAX_SPAWN_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds)
const BASE_RETRY_DELAY_MS: u32 = 200;

struct SpawnCtx {
    name: String,
    cols: u16,
    rows: u16,
    set_current_session_id: WriteSignal<Option<String>>,
    set_mode: WriteSignal<TerminalMode>,
    set_error: WriteSignal<Option<String>>,
    set_view_state: WriteSignal<ViewState>,
    on_session_created: Callback<Option<String>>,
    terminal_state_rc: Rc<RefCell<Option<TerminalState>>>,
    cols_signal: RwSignal<u16>,
    rows_signal: RwSignal<u16>,
    set_exit_info: WriteSignal<ExitInfo>,
    on_close: Callback<()>,
    working_directory: Option<String>,
    reconnect_trigger: RwSignal<bool>,
}

/// Helper function to spawn a new PTY session with shared state and retry logic
async fn spawn_new_session_with_state(ctx: SpawnCtx) {
    let SpawnCtx {
        name, cols, rows, set_current_session_id, set_mode, set_error,
        set_view_state, on_session_created, terminal_state_rc, cols_signal,
        rows_signal, set_exit_info, on_close, working_directory, reconnect_trigger,
    } = ctx;
    leptos::logging::log!("[Terminal] === Spawning new PTY session for: {} ({}x{}) working_dir={:?} ===", name, cols, rows, working_directory);

    let mut last_error = String::new();

    // Retry loop with exponential backoff
    for attempt in 0..MAX_SPAWN_RETRIES {
        if attempt > 0 {
            // Calculate exponential backoff delay: 200ms, 400ms, 800ms...
            let delay_ms = BASE_RETRY_DELAY_MS * (1 << (attempt - 1));
            leptos::logging::log!("[Terminal] Retry attempt {} of {} after {}ms delay", 
                attempt + 1, MAX_SPAWN_RETRIES, delay_ms);
            gloo_timers::future::TimeoutFuture::new(delay_ms).await;
        }
        
        leptos::logging::log!("[Terminal] Attempt {}: Calling pty_service::spawn_pty", attempt + 1);
        
        // Use a timeout for the spawn operation to prevent hanging
        let spawn_future = Box::pin(pty_service::spawn_pty(
            Some(name.clone()),
            Some(cols),
            Some(rows),
            working_directory.clone(),
            None, // TODO: pass project_id from context
        ));
        
        // Create a timeout future (5 seconds per attempt)
        let timeout_future = Box::pin(gloo_timers::future::TimeoutFuture::new(5000));
        
        // Race the spawn against the timeout
        let result = select(spawn_future, timeout_future).await;
        
        match result {
            Either::Left((spawn_result, _)) => {
                // Spawn completed (success or error)
                match spawn_result {
                    Ok(info) => {
                        leptos::logging::log!("[Terminal] ✓ PTY spawned successfully on attempt {}: id={}, name={}, size={}x{}", 
                            attempt + 1, info.id, info.name, cols, rows);
                        
                        let sid = info.id.clone();
                        set_current_session_id.set(Some(sid.clone()));
                        set_mode.set(TerminalMode::Connected);
                        
                        // Notify parent of new session ID
                        on_session_created.run(Some(sid.clone()));
                        
                        // Setup the event listener for PTY output
                        setup_pty_listener_with_state(ListenerCtx {
                            session_id: sid,
                            cols: cols as usize,
                            rows: rows as usize,
                            set_view_state,
                            initial_scrollback: None,
                            terminal_state_rc,
                            set_mode: Some(set_mode),
                            set_exit_info: Some(set_exit_info),
                            on_close: Some(on_close),
                            reconnect_trigger: Some(reconnect_trigger),
                        });
                        
                        leptos::logging::log!("[Terminal] ✓ Terminal ready and listening for output");
                        return; // Success! Exit the function
                    }
                    Err(e) => {
                        leptos::logging::error!("[Terminal] ✗ PTY spawn attempt {} failed: {}", attempt + 1, e);
                        last_error = e;
                        // Continue to next retry attempt
                    }
                }
            }
            Either::Right((_, _spawn_future)) => {
                // Timeout occurred
                leptos::logging::error!("[Terminal] ✗ PTY spawn attempt {} timed out after 5 seconds", attempt + 1);
                last_error = format!("Spawn timed out on attempt {}", attempt + 1);
                // Continue to next retry attempt
            }
        }
    }
    
    // All retries exhausted
    leptos::logging::error!("[Terminal] ✗ All {} PTY spawn attempts failed. Last error: {}", MAX_SPAWN_RETRIES, last_error);
    set_error.set(Some(format!("Failed after {} attempts: {}", MAX_SPAWN_RETRIES, last_error)));
    
    // Switch to demo mode
    set_mode.set(TerminalMode::Demo);
    set_view_state.set(ViewState::demo(cols as usize, rows as usize, &name));
}
