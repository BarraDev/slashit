//! Transport-neutral event emission.
//!
//! The queue executor, the PTY layer and the PR review flow all need to tell
//! *someone* what they are doing. In the desktop app that someone is a webview;
//! in a headless daemon there is no webview at all. Everything below the
//! command layer therefore emits through an [`EventSink`] rather than holding a
//! `tauri::AppHandle`.
//!
//! This is not a new idea in this codebase — `commands::pr` already threads a
//! `ProgressSink` through the apply flow so tests can observe progress without
//! a window. `EventSink` generalises that to the whole backend.

use std::sync::Arc;

/// Somewhere for a backend event to go.
///
/// Deliberately infallible: every existing call site discarded the emit result
/// with `let _ =`, because a UI that has gone away is not a reason to abort a
/// running agent. Implementations swallow or log their own failures.
pub trait EventSink: Send + Sync {
    /// Emit `payload` under the event name `event`.
    fn emit_json(&self, event: &str, payload: serde_json::Value);

    /// How this sink describes itself in diagnostics.
    fn describe(&self) -> &'static str;
}

/// Shared handle to whichever sink this process is using.
pub type SharedEventSink = Arc<dyn EventSink>;

/// Convenience for emitting a typed payload.
///
/// A separate extension trait because a generic method would make [`EventSink`]
/// non-object-safe, and the whole point is to store it as `dyn EventSink`.
pub trait EventSinkExt: EventSink {
    fn emit<T: serde::Serialize>(&self, event: &str, payload: &T) {
        match serde_json::to_value(payload) {
            Ok(value) => self.emit_json(event, value),
            Err(e) => eprintln!("[events] dropping `{event}`: payload did not serialise: {e}"),
        }
    }
}

impl<T: EventSink + ?Sized> EventSinkExt for T {}

/// Forwards to the webview. The only sink that exists in GUI mode.
pub struct TauriEventSink {
    handle: tauri::AppHandle,
}

impl TauriEventSink {
    pub fn new(handle: tauri::AppHandle) -> Self {
        Self { handle }
    }
}

impl EventSink for TauriEventSink {
    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        use tauri::Emitter;
        if let Err(e) = self.handle.emit(event, payload) {
            // A closed window is normal during shutdown; anything else is
            // worth seeing, and neither is worth failing the caller over.
            eprintln!("[events] failed to emit `{event}`: {e}");
        }
    }

    fn describe(&self) -> &'static str {
        "tauri"
    }
}

/// Writes events to stderr. The daemon's sink.
///
/// Deliberately one line per event with the name first, so `journalctl` and
/// `grep` are usable without a log parser.
pub struct LoggingEventSink {
    /// Events noisy enough to drown a log at info level.
    verbose: bool,
}

impl LoggingEventSink {
    /// Log every event, including per-token agent output.
    pub fn verbose() -> Self {
        Self { verbose: true }
    }

    /// Log lifecycle events but drop the streaming agent log spam.
    pub fn quiet() -> Self {
        Self { verbose: false }
    }

    /// Whether an event is streaming chatter rather than a state change.
    ///
    /// `agent-event` carries both: a `log` variant emitted per output chunk,
    /// and `phase_change` / `completed` / `error`, which a daemon operator
    /// genuinely wants.
    fn is_chatter(event: &str, payload: &serde_json::Value) -> bool {
        event == "agent-event"
            && matches!(
                payload.get("type").and_then(serde_json::Value::as_str),
                Some("log") | Some("tool_use")
            )
    }
}

impl EventSink for LoggingEventSink {
    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        if !self.verbose && Self::is_chatter(event, &payload) {
            return;
        }
        eprintln!("[event] {event} {payload}");
    }

    fn describe(&self) -> &'static str {
        if self.verbose {
            "logging(verbose)"
        } else {
            "logging"
        }
    }
}

/// Discards everything. For tests that exercise logic rather than reporting.
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn emit_json(&self, _event: &str, _payload: serde_json::Value) {}

    fn describe(&self) -> &'static str {
        "null"
    }
}

/// A sink that records what it was given, for assertions in tests.
#[cfg(test)]
pub struct RecordingEventSink {
    events: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
}

#[cfg(test)]
impl Default for RecordingEventSink {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl RecordingEventSink {
    pub fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn recorded(&self) -> Vec<(String, serde_json::Value)> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl EventSink for RecordingEventSink {
    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        self.events
            .lock()
            .unwrap()
            .push((event.to_string(), payload));
    }

    fn describe(&self) -> &'static str {
        "recording"
    }
}

/// The sink a headless process should use.
pub fn headless_sink(verbose: bool) -> SharedEventSink {
    if verbose {
        Arc::new(LoggingEventSink::verbose())
    } else {
        Arc::new(LoggingEventSink::quiet())
    }
}

/// A sink that discards. Handy as a default in tests and constructors.
pub fn null_sink() -> SharedEventSink {
    Arc::new(NullEventSink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_sink_accepts_anything_and_reports_itself() {
        let sink = null_sink();
        sink.emit_json("whatever", serde_json::json!({"a": 1}));
        assert_eq!(sink.describe(), "null");
    }

    #[test]
    fn a_typed_payload_reaches_the_sink_as_json() {
        #[derive(serde::Serialize)]
        struct Payload {
            task_id: String,
            progress: u8,
        }

        let sink = RecordingEventSink::new();
        sink.emit(
            "agent-event",
            &Payload {
                task_id: "abc".into(),
                progress: 42,
            },
        );

        let recorded = sink.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "agent-event");
        assert_eq!(recorded[0].1["task_id"], "abc");
        assert_eq!(recorded[0].1["progress"], 42);
    }

    #[test]
    fn the_quiet_logging_sink_drops_streaming_chatter_but_keeps_state_changes() {
        // `log` and `tool_use` fire per output chunk; a daemon running for days
        // should not write one line per token.
        assert!(LoggingEventSink::is_chatter(
            "agent-event",
            &serde_json::json!({"type": "log", "message": "x"})
        ));
        assert!(LoggingEventSink::is_chatter(
            "agent-event",
            &serde_json::json!({"type": "tool_use", "tool": "Bash"})
        ));

        for kept in ["phase_change", "completed", "error"] {
            assert!(
                !LoggingEventSink::is_chatter("agent-event", &serde_json::json!({"type": kept})),
                "`{kept}` is a state change and must survive the quiet filter"
            );
        }
    }

    #[test]
    fn other_event_names_are_never_treated_as_chatter() {
        assert!(!LoggingEventSink::is_chatter(
            "pty-output",
            &serde_json::json!({"type": "log"})
        ));
        assert!(!LoggingEventSink::is_chatter(
            "quit-requested",
            &serde_json::Value::Null
        ));
    }

    #[test]
    fn a_payload_that_cannot_serialise_is_dropped_rather_than_panicking() {
        // f64::NAN has no JSON representation; the emit must not unwind.
        #[derive(serde::Serialize)]
        struct Bad {
            #[serde(serialize_with = "always_fails")]
            value: u8,
        }
        fn always_fails<S: serde::Serializer>(_: &u8, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }

        let sink = RecordingEventSink::new();
        sink.emit("agent-event", &Bad { value: 1 });
        assert!(sink.recorded().is_empty());
    }
}
