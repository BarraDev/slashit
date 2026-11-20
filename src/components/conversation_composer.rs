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
//!
//! Conflict rule: when the store refuses a write because it holds a newer draft,
//! neither text replaces the other. Both are kept, writes and sends stop, and the
//! Human chooses which one to keep.

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
    /// The store refused a write because it holds a newer draft. `loaded` stays false,
    /// so nothing is written or sent, until the Human resolves the conflict.
    refused: bool,
    /// The newer stored draft that a refused write ran into, as last read. The local
    /// text is kept alongside it, unwritten, until the Human picks one; see [`keep_stored`]
    /// and [`resolve_keep_local`].
    conflict: Option<ConversationDraft>,
    /// A background failure that no open view of this Project has shown yet.
    error: Option<String>,
    /// Bumped when `sending`, `pending`, `loaded` or `conflict` changes, so a view that
    /// shows [`send_blocked`], [`outcome_unknown`] or [`draft_conflict`] updates when they do.
    revision: ArcRwSignal<u64>,
}

thread_local! {
    /// Where a background write's final failure is shown: the error line of the open composer of that Project.
    static ERROR_SINK: RefCell<Option<(String, WriteSignal<Option<String>>)>> = const { RefCell::new(None) };
    static SLOTS: RefCell<HashMap<String, Slot>> = RefCell::new(HashMap::new());
    static UNLOAD_HOOK: RefCell<Option<Closure<dyn FnMut()>>> = const { RefCell::new(None) };
}

fn with_slot<R>(project: &str, f: impl FnOnce(&mut Slot) -> R) -> R {
    let (result, changed) = SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        let slot = slots.entry(project.to_string()).or_default();
        let before = (slot.sending, slot.pending.clone(), slot.loaded, slot.conflict.clone());
        let result = f(slot);
        let changed = (before != (slot.sending, slot.pending.clone(), slot.loaded, slot.conflict.clone())).then(|| slot.revision.clone());
        (result, changed)
    });
    // Notified after the borrow ends, so nothing that reacts can re-enter the slots.
    if let Some(revision) = changed {
        revision.update(|value| *value += 1);
    }
    result
}

/// Subscribe the calling reactive scope to the send state of a Project.
fn track_send_state(project: &str) {
    with_slot(project, |slot| slot.revision.clone()).track();
}

/// The composer text of a Project, including edits not yet saved.
pub fn draft_text(project: &str) -> String {
    with_slot(project, |slot| slot.text.clone())
}

/// Whether a send must wait: one is in flight or unconfirmed, the stored draft
/// has not been read yet, or a draft conflict is unresolved. A send needs the
/// draft loaded, because its marker cannot be written before the stored sequence
/// is known, and it must not proceed while the Human has not chosen a draft.
pub fn send_blocked(project: &str) -> bool {
    track_send_state(project);
    with_slot(project, |slot| slot.sending || slot.pending.is_some() || !slot.loaded || slot.conflict.is_some())
}

/// Whether a send's outcome is unknown. Sending waits until the history shows it.
pub fn outcome_unknown(project: &str) -> bool {
    track_send_state(project);
    with_slot(project, |slot| !slot.sending && slot.pending.is_some())
}

/// The newer stored text that conflicts with this composer's unsaved text, if any.
pub fn draft_conflict(project: &str) -> Option<String> {
    track_send_state(project);
    with_slot(project, |slot| slot.conflict.as_ref().map(|stored| stored.text.clone()))
}

/// Show background save failures of `project` in the open composer. Replaced on every
/// mount. A failure of another Project is kept for that Project's next view, and one
/// kept for this Project is shown now.
pub fn set_error_sink(project: &str, sink: WriteSignal<Option<String>>) {
    ERROR_SINK.with(|cell| *cell.borrow_mut() = Some((project.to_string(), sink)));
    if let Some(message) = with_slot(project, |slot| slot.error.take()) {
        let _ = sink.try_set(Some(message));
    }
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

fn show_background_error(project: &str, message: String) {
    let shown = ERROR_SINK.with(|cell| match cell.borrow().as_ref() {
        Some((owner, sink)) if owner == project => sink.try_set(Some(message.clone())).is_none(),
        _ => false,
    });
    if !shown {
        with_slot(project, |slot| slot.error = Some(message));
    }
}

/// A debounced write, retried a bounded number of times. A failure that persists is
/// shown in the composer of its Project, so the text is never left unsaved without a warning.
pub async fn flush_in_background(project: String) {
    for attempt in 1..=BACKGROUND_SAVE_ATTEMPTS {
        // Nothing is written before the stored draft is read; the load path flushes once it is.
        if !with_slot(&project, |slot| slot.loaded) {
            return;
        }
        match flush(project.clone()).await {
            Ok(()) => return,
            // A newer stored draft makes `flush` unload the slot. Retrying cannot help, so show it now.
            Err(error) if !with_slot(&project, |slot| slot.loaded) => show_background_error(&project, error),
            Err(error) if attempt == BACKGROUND_SAVE_ATTEMPTS => {
                show_background_error(&project, format!("{error} SlashIt will keep the text and try again on your next change."));
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
    with_slot(project, |slot| apply_stored(slot, stored))
}

/// Apply a completed read of the stored draft. Text typed before the first read is
/// newer and kept. After a refused write neither text replaces the other: a stored
/// draft with different text becomes the conflict the Human resolves.
fn apply_stored(slot: &mut Slot, stored: Option<ConversationDraft>) -> Result<(), String> {
    // The first completed read is applied. A second read that finishes later may hold older text.
    if slot.loaded {
        return Ok(());
    }
    if slot.refused {
        if let Some(shown) = &slot.conflict {
            // A read that finishes late, or finds no draft, never replaces the newer draft already shown.
            if stored.as_ref().is_none_or(|stored| stored.seq < shown.seq) {
                return Ok(());
            }
        }
        match stored {
            Some(stored) if stored.text != slot.text => {
                slot.conflict = Some(stored);
                return Ok(());
            }
            // Same text: nothing to choose. The local text and marker are written above the stored sequence.
            Some(stored) => {
                slot.seq = slot.seq.max(stored.seq);
                slot.pending = slot.pending.take().or(stored.pending_send);
            }
            None => {}
        }
        slot.refused = false;
        slot.conflict = None;
        slot.dirty = true;
        slot.loaded = true;
        return Ok(());
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
    Ok(())
}

const CONFLICT_ERROR: &str = "A newer draft of this Project was saved elsewhere. Choose which version to keep.";

/// The handling of a write the store refused because it holds a newer draft. Nothing
/// is written or sent until the stored draft is read again and the conflict resolved.
fn refuse(slot: &mut Slot) {
    slot.loaded = false;
    slot.refused = true;
    slot.dirty = true;
}

/// Pending-send rule for both resolutions: the marker of this session's send wins,
/// because its message may already be saved and only the marker can settle it;
/// otherwise the stored draft's marker is carried. A resolution never drops a marker
/// and never creates one, so it cannot lose or repeat a message.
fn carried_marker(slot: &mut Slot, stored: &ConversationDraft) {
    slot.pending = slot.pending.take().or_else(|| stored.pending_send.clone());
}

/// Resolve a conflict by keeping the stored draft. The local text is dropped from
/// the composer and written nowhere. Returns `None` when there is no conflict or a
/// keep-local write is deciding it; otherwise whether the marker still needs a write.
fn keep_stored_slot(slot: &mut Slot) -> Option<bool> {
    if slot.saving {
        return None;
    }
    let stored = slot.conflict.take()?;
    carried_marker(slot, &stored);
    slot.text = stored.text.clone();
    slot.seq = slot.seq.max(stored.seq);
    slot.dirty = slot.pending != stored.pending_send;
    slot.refused = false;
    slot.loaded = true;
    Some(slot.dirty)
}

/// Keep the stored draft of a Project. Returns the composer text to show, or `None`
/// when nothing was resolved.
pub fn keep_stored(project: &str) -> Option<String> {
    let dirty = with_slot(project, keep_stored_slot)?;
    if dirty {
        arm_flush(project.to_string());
    }
    Some(draft_text(project))
}

/// The keep-local write: the local text at the sequence of the stored draft the Human
/// saw. The store accepts it only if nothing newer was written since, so it can never
/// overwrite a draft the Human has not seen.
fn begin_keep_local(slot: &mut Slot) -> Option<ConversationDraft> {
    if slot.saving {
        return None;
    }
    let stored = slot.conflict.clone()?;
    carried_marker(slot, &stored);
    slot.saving = true;
    Some(ConversationDraft { seq: stored.seq, text: slot.text.clone(), pending_send: slot.pending.clone() })
}

/// Apply the store's answer to a keep-local write. A refusal or failure leaves the
/// conflict open and the local text kept.
fn finish_keep_local(slot: &mut Slot, written: &ConversationDraft, result: &Result<bool, String>) {
    slot.saving = false;
    if matches!(result, Ok(true)) {
        slot.conflict = None;
        slot.refused = false;
        slot.loaded = true;
        slot.seq = slot.seq.max(written.seq);
        // Typing or a settled send during the write is newer than what was written.
        slot.dirty = slot.text != written.text || slot.pending != written.pending_send;
    }
}

/// Keep the local text of a Project over the stored draft. When another draft was
/// stored after the one shown, the store refuses; the newer draft is read and shown
/// as the conflict instead, and the error says so.
pub async fn resolve_keep_local(project: String) -> Result<(), String> {
    wait_for_write_slot(&project).await;
    let Some(draft) = with_slot(&project, begin_keep_local) else { return Ok(()) };
    let result = conversation_service::save_conversation_draft(project.clone(), draft.clone()).await;
    let dirty = with_slot(&project, |slot| {
        finish_keep_local(slot, &draft, &result);
        slot.dirty && slot.loaded
    });
    match result {
        Ok(true) => {
            if dirty {
                arm_flush(project);
            }
            Ok(())
        }
        Ok(false) => {
            load_draft(&project).await?;
            Err("Another draft was saved after the one shown. Choose again between your text and the newer draft.".to_string())
        }
        Err(error) => Err(format!("Your text could not be saved: {error}")),
    }
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
        let Some(draft) = with_slot(&project, next_write)? else { return Ok(()) };
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
                // The store holds a newer draft than this session knows of. It is read now and,
                // if its text differs, held next to the local text until the Human picks one.
                with_slot(&project, refuse);
                load_draft(&project).await?;
                if with_slot(&project, |slot| slot.conflict.is_some()) {
                    return Err(CONFLICT_ERROR.to_string());
                }
            }
            Err(error) => return Err(format!("The draft could not be saved: {error}")),
        }
    }
}

/// The next write of a Project's draft, `None` when nothing changed, or why no write
/// may run. Not loaded means the marker cannot be stored yet. That must be an error,
/// so a send never proceeds on `Ok`.
fn next_write(slot: &mut Slot) -> Result<Option<ConversationDraft>, String> {
    if slot.conflict.is_some() {
        return Err(CONFLICT_ERROR.to_string());
    }
    if !slot.loaded {
        return Err("The draft is not loaded yet. Wait for it to load before sending.".to_string());
    }
    if !slot.dirty {
        return Ok(None);
    }
    slot.dirty = false;
    slot.saving = true;
    slot.seq += 1;
    Ok(Some(ConversationDraft { seq: slot.seq, text: slot.text.clone(), pending_send: slot.pending.clone() }))
}

/// Start a write of every Project's unsaved draft. Used when the page is being hidden or
/// closed. Failures are shown like any background write's; nothing waits for the writes.
pub fn flush_all() {
    let projects: Vec<String> = SLOTS.with(|slots| slots.borrow().iter().filter(|(_, slot)| slot.dirty).map(|(id, _)| id.clone()).collect());
    for project in projects {
        spawn_local(flush_in_background(project));
    }
}

/// Flush unsaved drafts when the page is hidden. The writes are asynchronous, so the
/// webview may not finish them if the process exits first; that is why the debounce stays short.
/// A Project with an unresolved draft conflict writes nothing: its local text lives only
/// in memory and is lost when the app closes.
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

    fn revision(project: &str) -> u64 {
        with_slot(project, |slot| slot.revision.get_untracked())
    }

    fn history(id: Uuid, texts: &[&str]) -> ConversationSnapshot {
        snapshot(id, texts.iter().map(|text| entry(EntryKind::HumanMessage { text: (*text).into() })).collect())
    }

    #[test]
    fn an_unknown_outcome_notifies_the_view_and_clears_once_after_recovery() {
        let project = "outcome-transitions";
        let id = Uuid::new_v4();
        with_slot(project, |slot| {
            slot.loaded = true;
            slot.text = "now".into();
        });
        begin_send(project);
        with_slot(project, |slot| slot.pending = Some(pending(id, 1, "now")));
        assert!(!outcome_unknown(project) && send_blocked(project));
        // The send ends and its recovery read fails: nothing settles, so the outcome is unknown.
        let before = revision(project);
        end_send(project);
        assert!(revision(project) > before, "the panel must be told the outcome became unknown");
        assert!(outcome_unknown(project) && send_blocked(project));
        // A stale poll without the message proves nothing; sending stays blocked.
        assert_eq!(with_slot(project, |slot| settle_slot(slot, &history(id, &["earlier"]), false)), None);
        assert!(outcome_unknown(project));
        // "Check history" reads a history that holds the message.
        let before = revision(project);
        let saved = history(id, &["earlier", "now"]);
        assert_eq!(with_slot(project, |slot| settle_slot(slot, &saved, true)), Some(true));
        assert!(revision(project) > before, "the panel must be told the outcome is known");
        assert!(!outcome_unknown(project) && !send_blocked(project));
        // A second read of the same history settles nothing, so no message is restored or sent twice.
        assert_eq!(with_slot(project, |slot| settle_slot(slot, &saved, true)), None);
        assert_eq!(draft_text(project), "");
    }

    #[test]
    fn a_repeated_identical_message_is_confirmed_only_by_an_entry_after_its_own_marker() {
        let id = Uuid::new_v4();
        let mut slot = Slot { text: "yes".into(), pending: Some(pending(id, 2, "yes")), ..Slot::default() };
        // The earlier "yes" sits before the marker, so a stale read holding only it confirms nothing.
        assert_eq!(settle_slot(&mut slot, &history(id, &["earlier", "yes"]), false), None);
        assert_eq!(slot.text, "yes");
        // A recorded failure for the unanswered earlier message may come first; the new message still counts.
        let mut later = history(id, &["earlier", "yes"]);
        later.conversation.entries.push(entry(EntryKind::HumanMessage { text: "other".into() }));
        assert_eq!(settle_slot(&mut slot.clone(), &later, false), None);
        later.conversation.entries.push(entry(EntryKind::HumanMessage { text: "yes".into() }));
        assert_eq!(settle_slot(&mut slot, &later, false), Some(true));
    }

    fn stored(seq: u64, text: &str, marker: Option<ConversationPendingSend>) -> ConversationDraft {
        ConversationDraft { seq, text: text.into(), pending_send: marker }
    }

    /// A slot whose write at sequence 4 the store refused, after which it read `newer`.
    fn conflicted(project: &str, marker: Option<ConversationPendingSend>) {
        with_slot(project, |slot| {
            *slot = Slot { loaded: true, seq: 3, text: "local".into(), dirty: true, pending: marker, revision: slot.revision.clone(), ..Slot::default() };
            let write = next_write(slot).unwrap().unwrap();
            assert_eq!(write.seq, 4);
            slot.saving = false;
            refuse(slot);
            apply_stored(slot, Some(stored(9, "newer", None))).unwrap();
        });
    }

    #[test]
    fn an_open_conflict_blocks_sending_even_when_the_draft_is_marked_loaded() {
        let project = "conflict-blocks-send";
        with_slot(project, |slot| {
            slot.loaded = true;
            slot.text = "mine".into();
            slot.conflict = Some(ConversationDraft { seq: 5, text: "theirs".into(), pending_send: None });
        });
        assert!(send_blocked(project));
        with_slot(project, |slot| slot.conflict = None);
        assert!(!send_blocked(project));
    }

    #[test]
    fn a_refused_write_keeps_the_local_text_and_holds_the_stored_draft_without_writing_or_sending() {
        let project = "conflict-refusal";
        conflicted(project, None);
        assert_eq!(draft_text(project), "local");
        assert_eq!(draft_conflict(project).as_deref(), Some("newer"));
        assert!(send_blocked(project));
        // Typing during the conflict stays local, and no write of any kind may start.
        set_draft_text_in_slot(project, "local and more");
        assert_eq!(with_slot(project, next_write), Err(CONFLICT_ERROR.to_string()));
        assert_eq!(with_slot(project, |slot| (slot.seq, slot.saving)), (4, false));
        // A remount re-reads the same stored draft; the local text is still not replaced.
        with_slot(project, |slot| apply_stored(slot, Some(stored(9, "newer", None)))).unwrap();
        assert_eq!(draft_text(project), "local and more");
    }

    fn set_draft_text_in_slot(project: &str, text: &str) {
        with_slot(project, |slot| {
            slot.text = text.into();
            slot.dirty = true;
        });
    }

    #[test]
    fn keeping_the_local_text_writes_it_at_the_sequence_of_the_draft_shown() {
        let project = "conflict-keep-local";
        conflicted(project, None);
        let write = with_slot(project, begin_keep_local).unwrap();
        assert_eq!((write.seq, write.text.as_str()), (9, "local"));
        // A second click while the write is out does not start another.
        assert_eq!(with_slot(project, begin_keep_local), None);
        assert_eq!(with_slot(project, keep_stored_slot), None);
        with_slot(project, |slot| finish_keep_local(slot, &write, &Ok(true)));
        assert_eq!(draft_conflict(project), None);
        assert!(!send_blocked(project));
        assert_eq!(with_slot(project, |slot| (slot.text.clone(), slot.seq, slot.dirty)), ("local".to_string(), 9, false));
        // The next ordinary write is above the stored sequence.
        set_draft_text_in_slot(project, "local!");
        assert_eq!(with_slot(project, next_write).unwrap().unwrap().seq, 10);
    }

    #[test]
    fn a_keep_local_write_refused_by_a_newer_draft_shows_that_draft_and_overwrites_nothing() {
        let project = "conflict-keep-local-refused";
        conflicted(project, None);
        let write = with_slot(project, begin_keep_local).unwrap();
        // Another writer stored seq 12 meanwhile, so the store refuses the write at 9.
        with_slot(project, |slot| {
            finish_keep_local(slot, &write, &Ok(false));
            refuse(slot);
            apply_stored(slot, Some(stored(12, "newest", None))).unwrap();
        });
        assert_eq!(draft_conflict(project).as_deref(), Some("newest"));
        assert_eq!(draft_text(project), "local");
        assert!(send_blocked(project));
        // Choosing local again is checked against the newest draft shown.
        assert_eq!(with_slot(project, begin_keep_local).unwrap().seq, 12);
    }

    #[test]
    fn keeping_the_stored_draft_adopts_its_text_and_sequence_and_writes_nothing() {
        let project = "conflict-keep-stored";
        conflicted(project, None);
        assert_eq!(keep_stored(project).as_deref(), Some("newer"));
        assert_eq!(draft_conflict(project), None);
        assert!(!send_blocked(project));
        assert_eq!(with_slot(project, |slot| (slot.seq, slot.dirty)), (9, false));
        // The discarded local text is not written: there is no write at all.
        assert_eq!(with_slot(project, next_write), Ok(None));
        // A second resolution finds no conflict and changes nothing.
        assert_eq!(keep_stored(project), None);
        assert_eq!(draft_text(project), "newer");
    }

    #[test]
    fn a_newer_stored_draft_refreshes_an_open_conflict_and_an_older_read_never_does() {
        let project = "conflict-refresh";
        conflicted(project, None);
        with_slot(project, |slot| apply_stored(slot, Some(stored(11, "newest", None)))).unwrap();
        assert_eq!(draft_conflict(project).as_deref(), Some("newest"));
        // A read that started earlier and finishes later holds an older draft.
        with_slot(project, |slot| apply_stored(slot, Some(stored(9, "newer", None)))).unwrap();
        with_slot(project, |slot| apply_stored(slot, None)).unwrap();
        assert_eq!(draft_conflict(project).as_deref(), Some("newest"));
        assert_eq!(keep_stored(project).as_deref(), Some("newest"));
        assert_eq!(with_slot(project, |slot| slot.seq), 11);
    }

    #[test]
    fn a_pending_marker_survives_either_resolution_and_keeps_sending_blocked() {
        let id = Uuid::new_v4();
        let marker = pending(id, 1, "local");
        // Keep stored: the stored draft has no marker, so the carried one must be written.
        conflicted("conflict-marker-stored", Some(marker.clone()));
        assert_eq!(with_slot("conflict-marker-stored", keep_stored_slot), Some(true));
        assert_eq!(with_slot("conflict-marker-stored", |slot| slot.pending.clone()), Some(marker.clone()));
        assert!(send_blocked("conflict-marker-stored") && outcome_unknown("conflict-marker-stored"));
        let write = with_slot("conflict-marker-stored", next_write).unwrap().unwrap();
        assert_eq!((write.seq, write.pending_send), (10, Some(marker.clone())));
        // Keep local: the marker is written with the local text.
        conflicted("conflict-marker-local", Some(marker.clone()));
        let write = with_slot("conflict-marker-local", begin_keep_local).unwrap();
        assert_eq!(write.pending_send, Some(marker.clone()));
        with_slot("conflict-marker-local", |slot| finish_keep_local(slot, &write, &Ok(true)));
        assert!(send_blocked("conflict-marker-local"));
        // A second resolution neither drops nor repeats the marker, and a settled send clears it once.
        assert_eq!(with_slot("conflict-marker-local", begin_keep_local), None);
        let saved = history(id, &["earlier", "local"]);
        assert_eq!(with_slot("conflict-marker-local", |slot| settle_slot(slot, &saved, true)), Some(true));
        assert_eq!(with_slot("conflict-marker-local", |slot| settle_slot(slot, &saved, true)), None);
        assert!(!send_blocked("conflict-marker-local"));
    }

    #[test]
    fn a_stored_marker_is_carried_when_this_session_has_none() {
        let id = Uuid::new_v4();
        let theirs = pending(id, 3, "newer");
        let project = "conflict-marker-theirs";
        with_slot(project, |slot| {
            *slot = Slot { refused: true, text: "local".into(), dirty: true, revision: slot.revision.clone(), ..Slot::default() };
            apply_stored(slot, Some(stored(9, "newer", Some(theirs.clone())))).unwrap();
        });
        let write = with_slot(project, begin_keep_local).unwrap();
        assert_eq!(write.pending_send, Some(theirs.clone()));
        with_slot(project, |slot| finish_keep_local(slot, &write, &Err("offline".into())));
        // A failed write leaves the conflict open, the text local and the marker held.
        assert_eq!(draft_conflict(project).as_deref(), Some("newer"));
        assert_eq!(keep_stored(project).as_deref(), Some("newer"));
        assert_eq!(with_slot(project, |slot| (slot.pending.clone(), slot.dirty)), (Some(theirs), false));
    }

    #[test]
    fn a_conflict_in_one_project_leaves_another_untouched() {
        conflicted("conflict-a", None);
        with_slot("conflict-b", |slot| {
            slot.loaded = true;
            slot.text = "b".into();
            slot.dirty = true;
        });
        assert!(!send_blocked("conflict-b"));
        assert_eq!(draft_conflict("conflict-b"), None);
        assert_eq!(with_slot("conflict-b", next_write).unwrap().unwrap().text, "b");
        assert_eq!(draft_conflict("conflict-a").as_deref(), Some("newer"));
    }

    #[test]
    fn a_refused_write_whose_stored_text_matches_resumes_writing_above_it() {
        let mut slot = Slot { refused: true, seq: 3, text: "same".into(), dirty: true, ..Slot::default() };
        apply_stored(&mut slot, Some(stored(9, "same", None))).unwrap();
        assert!(slot.conflict.is_none() && slot.loaded && !slot.refused);
        assert_eq!(next_write(&mut slot).unwrap().unwrap().seq, 10);
    }

    #[test]
    fn text_typed_before_the_first_read_wins_and_a_late_second_read_is_ignored() {
        let mut slot = Slot { text: "typed".into(), dirty: true, ..Slot::default() };
        let stored = |text: &str, seq| Some(ConversationDraft { seq, text: text.into(), pending_send: None });
        assert!(apply_stored(&mut slot, stored("old", 4)).is_ok());
        assert_eq!((slot.text.as_str(), slot.seq, slot.dirty), ("typed", 4, true));
        // A duplicate mount whose read finishes later holds no newer truth than the first.
        assert!(apply_stored(&mut slot, stored("older", 2)).is_ok());
        assert_eq!((slot.text.as_str(), slot.seq), ("typed", 4));
    }

    #[test]
    fn a_background_failure_is_shown_only_in_its_own_project() {
        let (a_error, set_a_error) = signal(None::<String>);
        let (b_error, set_b_error) = signal(None::<String>);
        set_error_sink("errors-b", set_b_error);
        show_background_error("errors-a", "A failed".into());
        assert_eq!(b_error.get_untracked(), None);
        // The failure waits for Project A's next view.
        set_error_sink("errors-a", set_a_error);
        assert_eq!(a_error.get_untracked().as_deref(), Some("A failed"));
        assert_eq!(with_slot("errors-a", |slot| slot.error.clone()), None);
    }

    #[test]
    fn the_composer_clears_only_when_nothing_was_typed_after_the_sent_text() {
        assert_eq!(text_after_saved_send("hello", "hello"), Some(String::new()));
        assert_eq!(text_after_saved_send("hello\n", "hello"), Some(String::new()));
        assert_eq!(text_after_saved_send("hello and more", "hello"), None);
    }
}
