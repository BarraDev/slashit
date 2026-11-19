# Project Conversation composer and drafts

Status: implemented. Vocabulary follows
[product-model.md](../architecture/product-model.md). The Conversation history
itself is described in [project-conversation-history.md](project-conversation-history.md)
and is not changed by this note.

## Send key

`[conversations] send_shortcut` in `config.toml` chooses the send key:

- `modifier_enter` (default): Enter inserts a newline; Ctrl+Enter sends, or
  Cmd+Enter on macOS.
- `enter`: Enter sends; Shift+Enter inserts a newline. Ctrl/Cmd+Enter also
  sends.

The rule is `composer::key_action` in `src/components/conversation_composer.rs`.
It never sends while an input method is composing (`isComposing` or key code
229), never on Alt+Enter, and sends once per physical press: a key repeat is
suppressed. The Send button is unchanged. A hint under the message box names
the active key, and the textarea carries `aria-keyshortcuts`.

The choice is stored in `config.toml`, not in the browser, and is changed in
Settings > General.

## Drafts

A draft is the unsent text of one Project's composer. It is stored per Project
in `<data_dir>/conversations/<project-id>.draft` (JSON, owner-only permissions,
atomic replacement). The name has no `.json` suffix on purpose: the strict
Conversation scan at Project deletion treats every `*.json` as a document.
Drafts are machine-local. They are not part of the Conversation history, a
portable bundle, or the Project's repository.

Within a session, a process-wide cache keeps the text of every Project, so a
page change or a Project switch rebuilds the view without losing text.

### Write path

- Typing updates the cache at once and schedules a write after 250 ms of
  quiet. Typing never waits for the disk.
- One save chain runs per Project. A newer write waits for the one in flight,
  and the backend keeps only the highest sequence number, so an older
  asynchronous write cannot restore older text.
- Leaving the page and the `pagehide` event start a write of unsaved text.
  These writes are asynchronous: nothing waits for them, so a process that
  exits at once can lose them.
- Nothing is written before the stored draft has been read, so a sequence
  number is never chosen from a stale view. Text typed before that first read
  is newer and is kept.
- When the store refuses a write because it holds a higher sequence number,
  that is, another writer saved a newer draft, the composer reads the stored
  draft at once. Neither text replaces the other; see "Draft conflicts".
- A background write that keeps failing is shown in the composer of its own
  Project. If that Project has no open view, the failure is shown when it is
  next opened.

### Draft conflicts

A refused write never silently replaces text the Human typed.

- If the stored draft has the same text, there is nothing to choose: the
  composer adopts the stored sequence and writes above it.
- Otherwise both versions are kept: the local text stays in the composer, and
  a panel shows the stored draft. While the conflict is open, nothing is
  written for the Project (including the debounced, view-disposal and
  `pagehide` writes) and sending is blocked. Typing stays local.
- "Keep my text" writes the local text at the sequence of the stored draft
  shown. The store keeps a draft unless the stored sequence is higher, so the
  write succeeds only if nothing was stored after the draft the Human saw. If
  something was, the write is refused, the newer draft is read and shown, and
  the Human chooses again. Nothing is overwritten unseen.
- "Use the saved draft" puts the stored text in the composer and adopts its
  sequence. The local text is discarded and written nowhere.
- A later read never replaces the shown stored draft with an older one, and a
  read that finds no draft leaves the conflict open.
- Pending-send marker: both choices carry the same marker. This session's
  marker wins, because its message may already be saved and only the marker
  can settle it against the history; otherwise the stored draft's marker is
  carried. A resolution never drops or creates a marker, so it cannot lose or
  repeat a message, and sending stays blocked until the marker is settled.
- The conflict lives in the process-wide cache of the Project, so it survives
  a page change and a Project switch, and other Projects are unaffected. It
  does not survive a restart: until the Human chooses, the local text is held
  only in memory, and closing the app loses it. The panel says so.

### Sending

1. The composer records a pending send: the conversation id, the number of
   history entries before the send, and the message.
2. That marker is written before the message is sent. A failed write stops
   the send.
3. The message is sent. Whether it was saved is then decided from the history,
   not from the send's result: a Human message after the recorded entry count
   with the same text. The history is read again after a failed send.
4. When the history confirms the message, the composer text is cleared, but
   only if nothing was typed after the sent text, and the marker is removed.
5. When the history does not confirm the message and no send is in flight, the
   marker is dropped and the text stays for another send.
6. When the history cannot be read after a failed send, the outcome is
   unknown. Sending waits, and a "Check history" panel appears that reads it
   again; the panel goes away once a read settles the send.

### Send confirmation is not exactly-once

The message is matched by its text, after the recorded entry count. The
failure this allows: a second writer appends a Human message with identical
text between the read that recorded `entries_before` and the confirmation
read. A send that actually failed is then confirmed, its marker is removed,
and its text is cleared from the composer.

What prevents a second writer today:

- One live SlashIt process per state root. The desktop app claims the IPC
  endpoint before it hydrates any state and exits if another instance (the
  app or `slashitd`) holds it (`run` in `src-tauri/src/lib.rs`).
- One window. The only window is built by `main_window::create`, and requests
  for a new window are denied (`src-tauri/src/main_window.rs`).
- `send_project_message` in `src-tauri/src/commands/conversation.rs` holds
  the per-Project Conversation lock while it appends, and it is the only
  production code that appends a `HumanMessage`.
- The composer's `sending` and pending-marker gates allow one send per
  Project at a time across every view of that Project.

This is not a general exactly-once guarantee. A second writer to the same
Conversation, a second window, or a second process on the same state would
make the misconfirmation possible; any of them would require a per-send
identifier stored with the message.

A message is never sent without a stored marker, so a crash after the send can
still be settled from the history on the next start. Entries are append-only,
which is what makes the entry count a sound reference.

## Guarantees and limits

- A page change, a Project switch, and a normal navigation lose nothing.
- An application restart restores every draft that was written. A pending send
  is settled against the saved history before the text is shown again.
- An abrupt crash or power loss can lose the typing from the last 250 ms that
  had not reached the disk. Text typed before that is kept. The webview may not
  finish a write during a normal shutdown either, so the last moments before a
  close are best effort.
- During an unresolved draft conflict the local text is not written, so a
  restart loses it.
- A draft file that cannot be parsed blocks sending for that Project until the
  file is removed. This is deliberate: a send must never bypass the marker.
  The error names the file.
- Deleting a Project removes its draft.
- Not covered by desktop acceptance: IME composition (its rule is unit-tested
  only, because WebDriver cannot compose text) and a failed send before the
  history is saved (its branch is unit-tested only).
