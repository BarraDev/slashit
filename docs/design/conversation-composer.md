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
  that is, another writer saved a newer draft, nothing more is written or sent
  for the Project. The next read, when the Project is reopened, replaces the
  local text with the stored draft and says so.
- A background write that keeps failing is shown in the composer of its own
  Project. If that Project has no open view, the failure is shown when it is
  next opened.

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

The message is matched by its text, after the recorded entry count. Within one
app, sends to a Project are serialized, so a matching entry is this send's. A
Human message with the same text appended by another writer in the same
window would be taken for it; only a per-send identifier stored with the
message would rule that out.

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
- A draft file that cannot be parsed blocks sending for that Project until the
  file is removed. This is deliberate: a send must never bypass the marker.
  The error names the file.
- Deleting a Project removes its draft.
- Not covered by desktop acceptance: IME composition (its rule is unit-tested
  only, because WebDriver cannot compose text) and a failed send before the
  history is saved (its branch is unit-tested only).
