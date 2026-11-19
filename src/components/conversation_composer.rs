// SPDX-License-Identifier: MPL-2.0
//! The Project Conversation composer: the rule that decides when a key sends,
//! and the per-Project draft that survives page changes and restarts.
//!
//! The draft cache is global to the session, so a page change, which rebuilds
//! the Conversation view, does not lose text. Every write goes through one
//! save chain per Project, so an older asynchronous save cannot overtake a
//! newer one, and the backend keeps the highest sequence number as a second
//! guard.
//!
//! Integrity rule: a send first saves a marker that names the message and the
//! history it was sent after. The message is sent only once that marker is
//! stored. The draft is cleared only when the history confirms the Human
//! message was saved; a failed or ambiguous send keeps the text.

use crate::models::{ConversationDraft, ConversationEntryKind as EntryKind, ConversationPendingSend, ConversationSnapshot, SendShortcut};
use crate::services::conversation_service;
use gloo_timers::callback::Timeout;
use gloo_timers::future::TimeoutFuture;
use leptos::task::spawn_local;
use std::cell::RefCell;
use std::collections::HashMap;
use leptos::prelude::*;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;

/// Background writes that fail are retried this many times, this far apart, before the failure is shown.
const BACKGROUND_SAVE_ATTEMPTS: u32 = 3;
const BACKGROUND_RETRY_MS: u32 = 1_000;

/// How long typing must pause before the draft is written. Bounds the loss on an
/// abrupt crash to about this much typing; see the design note on drafts.
const SAVE_DEBOUNCE_MS: u32 = 250;

/// What the composer does with a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Send, and stop the key from inserting a newline.
    Send,
    /// A repeat of a send key: stop it from inserting a newline or sending again.
    Suppress,
    /// Leave the key to the textarea.
    Pass,
}

/// The keyboard rule. `primary` is Ctrl, or Cmd on macOS. `composing` is true
/// while an input method is composing, when Enter belongs to the IME.
pub fn key_action(shortcut: SendShortcut, enter: bool, primary: bool, shift: bool, alt: bool, composing: bool, repeat: bool) -> KeyAction {
    if !enter || alt || composing {
        return KeyAction::Pass;
    }
    let sends = match shortcut {
        // Enter inserts a newline; Ctrl/Cmd+Enter sends.
        SendShortcut::ModifierEnter => primary,
        // Enter sends; Shift+Enter inserts a newline. Ctrl/Cmd+Enter also sends.
        SendShortcut::Enter => primary || !shift,
    };
    match (sends, repeat) {
        (false, _) => KeyAction::Pass,
        (true, true) => KeyAction::Suppress,
        (true, false) => KeyAction::Send,
    }
}

/// Whether the Send action may run: text that is not blank, no send in progress,
/// and no unconfirmed earlier send.
pub fn can_send(text: &str, busy: bool, blocked: bool) -> bool {
    !text.trim().is_empty() && !busy && !blocked
}

/// True when the saved history shows the pending message as a Human message
/// recorded after the history the send started from.
pub fn send_was_saved(snapshot: &ConversationSnapshot, pending: &ConversationPendingSend) -> bool {
    snapshot.conversation.id == pending.conversation_id
        && snapshot.conversation.entries.iter().skip(pending.entries_before).any(|entry| {
            matches!(&entry.kind, EntryKind::HumanMessage { text } if *text == pending.message)
        })
}

/// The composer text once a send is confirmed saved: the sent text is removed
/// only when nothing was typed after it. `None` keeps the text as it is.
pub fn text_after_saved_send(current: &str, sent: &str) -> Option<String> {
    (current.trim() == sent).then(String::new)
}

/// The one-line hint under the composer. It names only the key that sends.
pub fn send_hint(shortcut: SendShortcut, macos: bool) -> &'static str {
    match (shortcut, macos) {
        (SendShortcut::ModifierEnter, true) => "Enter adds a new line. Cmd+Enter sends.",
        (SendShortcut::ModifierEnter, false) => "Enter adds a new line. Ctrl+Enter sends.",
        (SendShortcut::Enter, _) => "Enter sends. Shift+Enter adds a new line.",
    }
}

/// The `aria-keyshortcuts` value for the composer, matching [`send_hint`].
pub fn shortcut_keys(shortcut: SendShortcut, macos: bool) -> &'static str {
    match (shortcut, macos) {
        (SendShortcut::ModifierEnter, true) => "Meta+Enter",
        (SendShortcut::ModifierEnter, false) => "Control+Enter",
        (SendShortcut::Enter, _) => "Enter",
    }
}

/// Whether the platform's primary shortcut key is Cmd.
pub fn is_macos() -> bool {
    web_sys::window()
        .and_then(|window| window.navigator().platform().ok())
        .is_some_and(|platform| platform.contains("Mac"))
}

#[derive(Default, Clone)]
struct Slot {
    /// The composer text, including edits not yet written.
    text: String,
    /// The last sequence number this session stored, or the one the store held.
    seq: u64,
    /// The stored draft was read. Nothing is written before it is, so a stale
    /// store cannot be overwritten by a sequence number chosen too early.
    loaded: bool,
    /// The text, or the pending marker, changed since the last write started.
    dirty: bool,
    /// A write is in flight. Only one runs per Project.
    saving: bool,
    /// A debounced write is scheduled.
    timer_armed: bool,
    /// A send whose Human message may be saved.
    pending: Option<ConversationPendingSend>,
    /// A send is in flight, from any view of this Project.
    sending: bool,
}

thread_local! {
    /// Where a background write's final failure is shown: the open composer's error line.
    static ERROR_SINK: RefCell<Option<WriteSignal<Option<String>>>> = const { RefCell::new(None) };
    static SLOTS: RefCell<HashMap<String, Slot>> = RefCell::new(HashMap::new());
    static UNLOAD_HOOK: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
}

fn with_slot<R>(project: &str, f: impl FnOnce(&mut Slot) -> R) -> R {
    SLOTS.with(|slots| f(slots.borrow_mut().entry(project.to_string()).or_default()))
}

/// The composer text of a Project, including edits not yet saved.
pub fn draft_text(project: &str) -> String {
    with_slot(project, |slot| slot.text.clone())
}

/// Whether a send must wait: one is in flight or unconfirmed, or the stored
/// draft has not been read yet. A send needs the draft loaded, because its
/// marker cannot be written before the stored sequence is known.
pub fn send_blocked(project: &str) -> bool {
    with_slot(project, |slot| slot.sending || slot.pending.is_some() || !slot.loaded)
}

/// Whether a send's outcome is unknown. Sending waits until the history shows it.
pub fn outcome_unknown(project: &str) -> bool {
    with_slot(project, |slot| !slot.sending && slot.pending.is_some())
}

/// Show background save failures in the open composer. Replaced on every mount, so a
/// closed view's signal is never written.
pub fn set_error_sink(sink: WriteSignal<Option<String>>) {
    ERROR_SINK.with(|cell| *cell.borrow_mut() = Some(sink));
}

/// Record the composer text and schedule its write.
pub fn set_draft_text(project: &str, text: String) {
    with_slot(project, |slot| {
        slot.text = text;
        slot.dirty = true;
    });
    arm_flush(project.to_string());
}

fn arm_flush(project: String) {
    let armed = with_slot(&project, |slot| std::mem::replace(&mut slot.timer_armed, true));
    if armed {
        return;
    }
    Timeout::new(SAVE_DEBOUNCE_MS, move || {
        with_slot(&project, |slot| slot.timer_armed = false);
        spawn_local(flush_in_background(project));
    })
    .forget();
}

fn show_background_error(message: String) {
    ERROR_SINK.with(|cell| {
        if let Some(sink) = *cell.borrow() {
            let _ = sink.try_set(Some(message));
        }
    });
}

/// A debounced write, retried a bounded number of times. A failure that persists is
/// shown in the composer, so the text is never left unsaved without a warning.
async fn flush_in_background(project: String) {
    for attempt in 1..=BACKGROUND_SAVE_ATTEMPTS {
        // Nothing is written before the stored draft is read; the load path flushes once it is.
        if !with_slot(&project, |slot| slot.loaded) {
            return;
        }
        match flush(project.clone()).await {
            Ok(()) => return,
            // A newer stored draft makes `flush` unload the slot. Retrying cannot help, so show it now.
            Err(error) if !with_slot(&project, |slot| slot.loaded) => show_background_error(error),
            Err(error) if attempt == BACKGROUND_SAVE_ATTEMPTS => {
                show_background_error(format!("{error} SlashIt will keep the text and try again on your next change."));
            }
            Err(_) => TimeoutFuture::new(BACKGROUND_RETRY_MS).await,
        }
    }
}

/// Read the saved draft of a Project once per session. Text typed before the
/// read finishes is newer, so it is kept and the saved text is not applied over it.
pub async fn load_draft(project: &str) -> Result<(), String> {
    if with_slot(project, |slot| slot.loaded) {
        return Ok(());
    }
    let stored = conversation_service::get_conversation_draft(project.to_string()).await?;
    with_slot(project, |slot| {
        // The first completed read is applied. A second read that finishes later may hold older text.
        if slot.loaded {
            return;
        }
        if let Some(stored) = stored {
            slot.seq = slot.seq.max(stored.seq);
            if !slot.dirty {
                slot.text = stored.text;
            }
            if slot.pending.is_none() {
                slot.pending = stored.pending_send;
            }
        }
        slot.loaded = true;
    });
    Ok(())
}

async fn wait_for_write_slot(project: &str) {
    while with_slot(project, |slot| slot.saving) {
        TimeoutFuture::new(10).await;
    }
}

/// Write the draft until nothing newer is waiting. Returns an error when a write failed.
pub async fn flush(project: String) -> Result<(), String> {
    loop {
        wait_for_write_slot(&project).await;
        // Not loaded means the marker cannot be stored yet. That must be an error, so a send never proceeds on Ok.
        if !with_slot(&project, |slot| slot.loaded) {
            return Err("The draft is not loaded yet. Wait for it to load before sending.".to_string());
        }
        let job = with_slot(&project, |slot| {
            if !slot.dirty {
                return None;
            }
            slot.dirty = false;
            slot.saving = true;
            slot.seq += 1;
            Some(ConversationDraft { seq: slot.seq, text: slot.text.clone(), pending_send: slot.pending.clone() })
        });
        let Some(draft) = job else { return Ok(()) };
        let result = conversation_service::save_conversation_draft(project.clone(), draft).await;
        with_slot(&project, |slot| {
            slot.saving = false;
            if result.is_err() {
                // Keep the text and try again on the next change.
                slot.dirty = true;
            }
        });
        match result {
            Ok(true) => {}
            Ok(false) => {
                // The store holds a newer draft than this session knows of. Reading it again
                // (reopening the Project) adopts its sequence; until then nothing is written or sent.
                with_slot(&project, |slot| {
                    slot.loaded = false;
                    slot.dirty = true;
                });
                return Err("The draft store holds a newer draft. Reopen the Project to load it before sending.".to_string());
            }
            Err(error) => return Err(format!("The draft could not be saved: {error}")),
        }
    }
}

/// Write every Project's unsaved draft. Used when the page is being hidden or closed.
pub fn flush_all() {
    let projects: Vec<String> = SLOTS.with(|slots| slots.borrow().iter().filter(|(_, slot)| slot.dirty).map(|(id, _)| id.clone()).collect());
    for project in projects {
        spawn_local(async move {
            let _ = flush(project).await;
        });
    }
}

/// Flush unsaved drafts when the page is hidden. The webview may not finish the
/// writes if the process exits first, which is why the debounce stays short.
pub fn install_unload_flush() {
    UNLOAD_HOOK.with(|hook| {
        if hook.borrow().is_some() {
            return;
        }
        let Some(window) = web_sys::window() else { return };
        let callback = Closure::<dyn FnMut()>::new(flush_all);
        let target: &web_sys::EventTarget = window.as_ref();
        if target.add_event_listener_with_callback("pagehide", callback.as_ref().unchecked_ref()).is_ok() {
            *hook.borrow_mut() = Some(callback);
        }
    });
}

/// Mark a send as in flight. Other views of this Project then refuse to send.
pub fn begin_send(project: &str) {
    with_slot(project, |slot| slot.sending = true);
}

/// Record the marker of the send that is starting. The caller stores it (see
/// [`flush`]) before the message is sent.
pub fn arm_pending_send(project: &str, pending: ConversationPendingSend) {
    with_slot(project, |slot| {
        slot.pending = Some(pending);
        slot.dirty = true;
    });
    arm_flush(project.to_string());
}

/// Mark the send as no longer in flight, without any outcome.
pub fn end_send(project: &str) {
    with_slot(project, |slot| slot.sending = false);
}

/// Settle a pending send against the history. Clears the text and the marker
/// when the history shows the Human message saved. The marker is dropped for
/// a message that is absent only when `read_after_send` is true, meaning the
/// snapshot was read after the send ended. A snapshot from an older read, such
/// as a poll that began before the send, can predate the save and must not be
/// taken as proof that the message was never sent. Returns true when the
/// composer text changed.
pub fn settle_pending_send(project: &str, snapshot: &ConversationSnapshot, read_after_send: bool) -> bool {
    let outcome = with_slot(project, |slot| settle_slot(slot, snapshot, read_after_send));
    // The store must see the marker's removal too, so the write is scheduled on every change.
    if outcome.is_some() {
        arm_flush(project.to_string());
    }
    outcome.unwrap_or(false)
}

/// The settle rule on one Project's slot. `None` leaves the slot untouched; otherwise the
/// marker is settled and the value says whether the composer text changed.
fn settle_slot(slot: &mut Slot, snapshot: &ConversationSnapshot, read_after_send: bool) -> Option<bool> {
    let pending = slot.pending.clone()?;
    if send_was_saved(snapshot, &pending) {
        slot.pending = None;
        let cleared = text_after_saved_send(&slot.text, &pending.message);
        if let Some(text) = &cleared {
            slot.text = text.clone();
        }
        slot.dirty = true;
        return Some(cleared.is_some());
    }
    if read_after_send && !slot.sending {
        slot.pending = None;
        slot.dirty = true;
        return Some(false);
    }
    None
}

/// Stop a pending marker from waiting for a confirmation that never comes.
pub fn forget_pending_send(project: &str) {
    with_slot(project, |slot| {
        slot.pending = None;
        slot.dirty = true;
    });
    arm_flush(project.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ConversationEntry, ConversationRole};
    use uuid::Uuid;

    fn entry(kind: EntryKind) -> ConversationEntry {
        ConversationEntry { id: Uuid::new_v4(), role: ConversationRole::Human, kind, created_at: String::new() }
    }

    fn snapshot(id: Uuid, entries: Vec<ConversationEntry>) -> ConversationSnapshot {
        serde_json::from_value(serde_json::json!({
            "conversation": {
                "id": id, "project_id": Uuid::nil(), "revision": 1,
                "created_at": "", "updated_at": "",
                "entries": serde_json::to_value(&entries).unwrap(), "actions": [],
            },
            "coordinator_live": false, "worker_live": false, "run_status": null,
        }))
        .unwrap()
    }

    fn pending(id: Uuid, entries_before: usize, message: &str) -> ConversationPendingSend {
        ConversationPendingSend { conversation_id: id, entries_before, message: message.into() }
    }

    #[test]
    fn enter_inserts_a_newline_and_ctrl_or_cmd_enter_sends_once_by_default() {
        use SendShortcut::ModifierEnter as M;
        assert_eq!(key_action(M, true, false, false, false, false, false), KeyAction::Pass);
        assert_eq!(key_action(M, true, true, false, false, false, false), KeyAction::Send);
        // Holding the key down sends once, not once per repeat.
        assert_eq!(key_action(M, true, true, false, false, false, true), KeyAction::Suppress);
        assert_eq!(key_action(M, true, false, true, false, false, false), KeyAction::Pass);
    }

    #[test]
    fn enter_sends_and_shift_enter_inserts_a_newline_in_the_alternative_mode() {
        use SendShortcut::Enter as E;
        assert_eq!(key_action(E, true, false, false, false, false, false), KeyAction::Send);
        assert_eq!(key_action(E, true, false, true, false, false, false), KeyAction::Pass);
        assert_eq!(key_action(E, true, true, false, false, false, false), KeyAction::Send);
        assert_eq!(key_action(E, true, false, false, false, false, true), KeyAction::Suppress);
    }

    #[test]
    fn an_input_method_composition_never_sends_and_other_keys_pass() {
        for shortcut in [SendShortcut::ModifierEnter, SendShortcut::Enter] {
            assert_eq!(key_action(shortcut, true, true, false, false, true, false), KeyAction::Pass);
            assert_eq!(key_action(shortcut, true, false, false, false, true, false), KeyAction::Pass);
        }
        assert_eq!(key_action(SendShortcut::Enter, false, true, false, false, false, false), KeyAction::Pass);
        assert_eq!(key_action(SendShortcut::Enter, true, false, false, true, false, false), KeyAction::Pass);
    }

    #[test]
    fn blank_busy_or_blocked_text_cannot_send() {
        assert!(can_send("hello", false, false));
        assert!(!can_send("   \n", false, false));
        assert!(!can_send("hello", true, false));
        assert!(!can_send("hello", false, true));
    }

    #[test]
    fn a_pending_send_is_saved_only_when_its_human_message_follows_the_recorded_history() {
        let id = Uuid::new_v4();
        let before = vec![entry(EntryKind::HumanMessage { text: "earlier".into() })];
        let mut after = before.clone();
        after.push(entry(EntryKind::HumanMessage { text: "now".into() }));
        let marker = pending(id, before.len(), "now");
        assert!(send_was_saved(&snapshot(id, after.clone()), &marker));
        // The same text sent earlier does not count as this send.
        let mut repeated = before.clone();
        repeated.push(entry(EntryKind::HumanMessage { text: "now".into() }));
        assert!(!send_was_saved(&snapshot(id, before.clone()), &marker));
        assert!(!send_was_saved(&snapshot(Uuid::new_v4(), after), &marker));
        assert!(!send_was_saved(&snapshot(id, repeated), &pending(id, 2, "now")));
    }

    #[test]
    fn the_hint_names_the_key_that_sends_for_each_mode_and_platform() {
        assert_eq!(send_hint(SendShortcut::ModifierEnter, false), "Enter adds a new line. Ctrl+Enter sends.");
        assert_eq!(send_hint(SendShortcut::ModifierEnter, true), "Enter adds a new line. Cmd+Enter sends.");
        assert_eq!(send_hint(SendShortcut::Enter, true), "Enter sends. Shift+Enter adds a new line.");
        assert_eq!(shortcut_keys(SendShortcut::ModifierEnter, true), "Meta+Enter");
        assert_eq!(shortcut_keys(SendShortcut::Enter, false), "Enter");
    }

    #[test]
    fn a_delayed_snapshot_that_predates_the_save_never_proves_the_message_absent() {
        // The rule, as settle applies it: absence is conclusive only after a read taken after the send ended.
        let id = Uuid::new_v4();
        let before = vec![entry(EntryKind::HumanMessage { text: "earlier".into() })];
        let marker = pending(id, before.len(), "now");
        assert!(!send_was_saved(&snapshot(id, before.clone()), &marker));
        let mut after = before.clone();
        after.push(entry(EntryKind::HumanMessage { text: "now".into() }));
        assert!(send_was_saved(&snapshot(id, after), &marker));
    }

    #[test]
    fn an_absent_message_drops_the_marker_only_after_a_read_taken_after_the_send() {
        let id = Uuid::new_v4();
        let before = vec![entry(EntryKind::HumanMessage { text: "earlier".into() })];
        let absent = snapshot(id, before.clone());
        let mut slot = Slot { text: "now".into(), pending: Some(pending(id, before.len(), "now")), ..Slot::default() };
        // A read that may predate the save leaves the marker in place.
        assert_eq!(settle_slot(&mut slot, &absent, false), None);
        assert!(slot.pending.is_some());
        // A read taken while the send is still in flight is not conclusive either.
        slot.sending = true;
        assert_eq!(settle_slot(&mut slot, &absent, true), None);
        assert!(slot.pending.is_some());
        // Once the send has ended, the absence is conclusive and the text is kept.
        slot.sending = false;
        assert_eq!(settle_slot(&mut slot, &absent, true), Some(false));
        assert!(slot.pending.is_none());
        assert_eq!(slot.text, "now");
    }

    #[test]
    fn a_saved_message_clears_the_text_and_the_marker_from_any_read() {
        let id = Uuid::new_v4();
        let before = vec![entry(EntryKind::HumanMessage { text: "earlier".into() })];
        let mut after = before.clone();
        after.push(entry(EntryKind::HumanMessage { text: "now".into() }));
        let mut slot = Slot { text: "now".into(), pending: Some(pending(id, before.len(), "now")), ..Slot::default() };
        assert_eq!(settle_slot(&mut slot, &snapshot(id, after), false), Some(true));
        assert!(slot.pending.is_none());
        assert_eq!(slot.text, "");
    }

    #[test]
    fn the_composer_clears_only_when_nothing_was_typed_after_the_sent_text() {
        assert_eq!(text_after_saved_send("hello", "hello"), Some(String::new()));
        assert_eq!(text_after_saved_send("hello\n", "hello"), Some(String::new()));
        assert_eq!(text_after_saved_send("hello and more", "hello"), None);
    }
}
