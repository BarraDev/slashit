# Non-destructive Project removal

Status: **proposal**. Nothing here is implemented. Statements about current
behavior describe `main` and cite the code; everything else is marked
**proposed**. Vocabulary follows [product-model.md](product-model.md); storage
locations follow [state-locations.md](state-locations.md).

## Problem

The only way to take a Project out of the sidebar today is `delete_project`
(`src-tauri/src/commands/project.rs`). It is destructive by design: after
refusing on live work it removes the Project from `config.toml`, writes
cleanup records, deletes the Project's Task board files, and deletes its
primary Conversation (`delete_project_committed`,
`Storage::delete_project_conversation`). Tasks, their Activity timelines and
Human Review history live in the board file, so they are lost with it.

People also want to stop seeing a repository without losing anything: a
finished client project, a repository that moved, a mistaken registration.
They currently have no option between "keep it forever" and "erase its
history". This proposal adds **Remove from SlashIt** (archive), which only
changes whether the Project is *active*, and keeps `delete_project` as the
separate, explicit erase.

## Current facts the design rests on

- A Project is a record in `config.toml` (`AppConfig.projects`, keyed by id
  string) holding `id`, `name`, `repository_id`, `scope`, `state_location`,
  `base`, agent settings (`domain::Project`). It points at a Repository
  (`domain::Repository`: `local_path`, remote) by `repository_id`.
- `create_repository` always mints a fresh `Repository.id` and `create_project`
  a fresh `Project.id`. **Nothing compares paths**, so registering the same
  folder twice yields two independent Projects with separate boards (the
  state-locations doc relies on this: the Project UUID keeps two Projects on
  one repository from sharing a board).
- Task state is per Project: the board `tasks.toml` at
  `<data_dir>/projects/<project-key>/<project-id>/` or
  `<repo>/.slashit/<project-id>/`. The key is derived from the path hash
  once and **stored** with the Project so a moved Project keeps its state.
  Tasks carry their own Activity timeline, Human Review decisions, delivery
  facts and checkout references.
- The primary Conversation is a separate file under `conversations_dir()`
  (`primary_conversation_file(project_id)`), referenced from Project id.
- Task Checkouts are Git worktrees under `<data_dir>/worktrees/<project-key>/`
  and are registered in the repository's `.git/worktrees`.
- Secrets (`AgentConfig::api_key`) are in `config.toml`, mode 0600; daemon
  tokens and credentials live in `config_dir`, not per Project.
- Deletion already has the right locking template: the Project Conversation
  mutation lock, then the Project lifecycle gate in write, then every Task's
  lifecycle lease, then the board write guard
  (`lock_project_conversation_for_deletion`, `project_gate.write`,
  `hold_project_task_leases`). Live work is detected by
  `live_work_refusal` (running agent via `ExecutionOwnership`, and
  `DeliveriesInFlight`), a live Project Conversation Run, and
  `lifecycle::project_deletion_refusal`.
- Startup treats the config's Project set as authoritative for cleanup:
  recorded Task/Conversation deletion records are executed for Projects the
  config no longer names (`app_core.rs`). That behavior is why archiving
  cannot be "just drop the entry from `projects`" (see Risks).

## Design

### 1. Archived Project record (proposed)

Add a second registry section to `config.toml`, `archived_projects`, keyed by
Project id, whose values are:

```text
ArchivedProject {
  project: Project,            // the full record, byte-for-byte as it was
  repository: Repository,      // the Repository record it pointed at
  identity: RepositoryIdentity // see section 3
  archived_at: DateTime<Utc>,
  board_files: Vec<PathBuf>,   // the routes captured at removal (as delete does)
}
```

Keeping the whole `Project` (including `state_location`, the stored project
key, `base`, `scope`, agent settings) means a restored Project is
indistinguishable from the original and the stored key keeps pointing at the
same state directories. Keeping `board_files` is the same safeguard delete
uses: once the Project is out of the active set, the config can no longer
answer where its board lives.

Removal **moves** the entry: remove from `projects`, insert into
`archived_projects`, and persist both in one `Storage::update_config`
transaction, so a crash leaves it in exactly one section. The Repository
record is likewise moved out of `repositories` only if no other active Project
references it.

No file, Git ref, worktree, Conversation, credential or board is touched by
removal. This is the central invariant and it is testable (below).

### 2. What is preserved, and where

| Data | Today | After removal |
|---|---|---|
| Tasks, Activity, Human Review history, delivery facts | board file | untouched on disk, **not loaded** into the active board while archived |
| Primary Conversation (ordered history) | conversation file | untouched on disk |
| Task Checkouts and branches | worktree dir + `.git/worktrees` | untouched; nothing runs in them |
| Project settings, base, scope | `config.toml` | in `archived_projects` |
| API key / daemon tokens | `config_dir` | untouched; the archived record keeps the Project's own `agent_config` and stays in the 0600 file |
| Pending PR / integration records | on the Tasks | on the Tasks, untouched |

Because archived Tasks are not in the active in-memory board, the queue, the
scheduler, attention aggregation, Needs You, CLI `list`, and PR status polling
see nothing of them. Startup must load boards only for active Projects and
must **not** treat the board files of an archived Project as orphans. Any
sweep that reasons "Project absent from `projects`" (startup cleanup records,
orphan-board handling) must consult `archived_projects` first. This is the main
code risk; section "Test plan" pins it.

Activity and Human Review need no separate preservation work: they are fields
of the Task records, so preserving the board preserves them. Disk accounting
(`storage_accounting`) already measures by directory and should list archived
state separately ("archived Projects") so removal does not look like space was
freed.

### 3. Re-adding the same repository

Define a **repository identity** used only to detect "this is the same
repository":

1. canonical absolute path (symlinks resolved, trailing separators removed;
   case-folded only on case-insensitive filesystems), and
2. a secondary fingerprint: the repository's root commit set (first root
   commit id) plus `remote_url` when present.

Rules at registration (`create_repository`, then `create_project`):

- Path matches an **active** Project's repository: refuse, naming that
  Project ("already added as <name>"). This closes today's silent duplicate
  registration for new Projects. Existing duplicates already on disk are left
  alone and reported, not merged.
- Path matches an **archived** Project: do not create a new Project. Offer
  **Restore** (default) or, explicitly, **Add as new Project**. Restore
  re-activates the same Project id with the same board, Conversation and
  stored project key, re-checks the repository (below) and moves the record
  back to `projects` in one transaction.
- Path differs but the root commit and remote match an archived Project (the
  directory was moved): offer Restore *and re-point*, which updates the
  Repository `local_path` while keeping the stored project key, exactly the
  "moved project keeps its state until you migrate it deliberately" rule from
  the state-locations doc. Never auto-pick; a fingerprint match is a
  suggestion shown to the person.
- Root commit differs at the same path (different repository now at that
  path): treat as a conflict. Restore is refused with an explanation;
  "Add as new Project" remains available and creates a fresh id.
- A Project id is unique across `projects ∪ archived_projects`; restore and
  add both assert this inside the config transaction so two concurrent
  re-additions (or a CLI and the GUI) cannot both win.
- "Add as new Project" at a path with an archived Project is allowed but does
  not delete or adopt the archived state; the two ids stay separate and the
  Projects list shows the archive entry still restorable. The board key
  derives from the path hash, but the state directory also includes the
  Project id, so the two cannot share a board.

Restore also validates, before activating, that:

- the repository directory exists and is a usable Git repository (same check
  the registration path uses); otherwise the restore is refused and the
  archive is unchanged;
- the board file parses under the current board contract; an unreadable
  board is surfaced with the file path exactly as startup does today, and the
  Project is not activated against it;
- `scope` still names a Workspace that exists; if not, the Project is restored
  as `Standalone` and the person is told (the record is not left pointing at a
  deleted Workspace). A Workspace with only archived members is currently
  deletable because membership is read from active Projects; the proposal
  makes the workspace-delete check include archived members (open decision 4).

Task Checkouts of a restored Project resume through the existing Adoption and
"checkout whose directory is gone" rules; restore does not repair them.

### 4. Refusals

Removal takes the same locks in the same order as deletion (Conversation lock
→ Project gate write → all Task leases → board write guard), then refuses,
before changing anything, when any of these hold. Messages reuse the existing
wording style and name the Task:

- an agent is running for a Task of the Project (`live_work_refusal`);
- a PR is being opened (`DeliveriesInFlight`), or a Task owns a PR helper, AI
  review or PR-review fix;
- the Project Conversation has a live Run (`project_run_is_live`);
- a Task is queued or In Progress / AI Review (the queue would otherwise pick
  it up or lose track of it) — the person stops or moves it first;
- a Task records an unfinished integration, an unfinished restack of a
  published branch, `cleanup_in_flight`, or an unanswered
  `pr_create_unconfirmed` request: these are in-progress lifecycle
  operations whose resume/discard UI lives on the active board, so removing
  the Project would hide a decision that is waiting for a person;
- a pending Project Task/Conversation deletion record exists for the id;
- the board cannot be read (it cannot be proven that nothing is mid-flight);
- the configuration cannot be written. A write that is visible but not
  confirmed durable (`PublishedNotDurable`) is reported as "removed, durability
  unconfirmed", memory follows the file (`adopt_published`), and the next
  start's `reconfirm_config_durable` settles it, as for creation and deletion.

Deliberately **not** a refusal, unlike `delete_project`: a Task that still has
a Task Checkout. The checkout is left in place, so nothing is stranded; deletion
refuses it only because it erases the Task records that reference the checkout.

Restore and the "Add as new" path take the same gate and refuse while a
deletion of the same id is pending. Archiving an already archived id is an
idempotent no-op that reports "already removed".

### 5. Failure and restart behavior

The change is a single config transaction, so there is no multi-file step to
recover, which is the point of choosing a registry move over deletion's
record-and-sweep protocol:

- failure before publish: config unchanged, memory unchanged, error returned;
- published-but-not-durable: as above;
- crash after publish: next start reads `archived_projects`, loads no board
  for it, runs no cleanup on it;
- crash before publish: Project still active, nothing lost.

A downgrade to a build that does not know `archived_projects` would drop the
section on its next config save (unknown top-level keys are not preserved for
config). Proposed mitigation: bump a config marker so an older build refuses to
save, matching the board's refuse-to-overwrite stance; see open decision 5.

### 6. Surfaces

- Backend command `archive_project` (name: `remove_project_from_registry`),
  GUI-only through `GuiMutation`, like `delete_project`. Returns the archived
  record or a refusal. `restore_project` and `list_archived_projects` are its
  companions. The CLI/IPC control channel gets read-only listing at most;
  archive/restore are not agent-reachable (they hide and reveal work, and
  restore re-enables execution).
- `delete_project` keeps its behavior but, for a Project that is archived,
  works on the archived record (so "erase" remains possible from the archive
  list) and is the only path that removes data.

### 7. UI explanation

Project settings get two clearly separate actions.

**Remove from SlashIt** (default, not styled as danger):

> Remove *<name>* from SlashIt? It will disappear from your Projects and stop
> running. SlashIt keeps its Tasks, history, reviews and Conversation, and
> leaves your repository, branches and Task Checkouts exactly as they are.
> Add the same folder again later to restore it. This does not delete
> anything and does not touch GitHub.

If refused: a dialog listing each blocker with the Task, and what to do ("Stop
the running agent on *Fix login*", "Resume or discard the unfinished
integration on *X*").

**Delete permanently…** (danger style, existing behavior): states that Task
records, Activity, reviews and the Conversation are erased, that repository
files, branches and recovery refs are not, and requires typing the Project name.

A "Removed Projects" list (Settings) shows name, path, removed-at, number of
Tasks, and Restore / Delete permanently. Adding a folder that matches an
entry shows: "This folder was removed from SlashIt on <date>. Restore it with
its <n> Tasks, or add it as a new Project." Copy uses "Workspace" only for the
Product Workspace and "Task Checkout" per the vocabulary.

## Test plan

Backend unit tests next to `commands/project.rs` (temp `Storage`, as the
existing deletion tests do):

1. **Removal preserves everything.** Seed a Project with Tasks (Activity, Human
   Review decisions), a Conversation, a checkout reference. Remove. Assert
   byte-identical board and Conversation files, config no longer lists it
   active, `archived_projects` holds the exact `Project`, nothing deleted,
   no Git refs/worktrees changed.
2. **Active surfaces forget it.** After removal and after a fresh
   `app_core::build_state`, the Project's Tasks are in no queue, attention,
   CLI listing or PR poll; its boards are not reported as orphans or deleted.
3. **Restart.** Remove, rebuild state (simulated restart), assert still
   archived and files intact; restore, rebuild, assert the same Project id,
   Tasks, Activity, Human Review history and Conversation are back.
4. **Re-addition.** Same path → restore offered, same id and project key, no
   second Project; moved path with matching root commit → re-point offered,
   key unchanged; different repository at the path → restore refused,
   add-as-new creates a fresh id and does not touch the archive; active
   duplicate → refused.
5. **Identity uniqueness under concurrency.** Two simultaneous restores and a
   concurrent create at one path: exactly one wins, the config never holds one
   id in both sections.
6. **Refusals.** One test per condition in section 4 (running agent, delivery
   in flight, live Conversation Run, queued/In Progress Task, pending
   integration, pending restack, `cleanup_in_flight`, `pr_create_unconfirmed`,
   pending deletion record, unreadable board). Each asserts config and files
   unchanged. A Task that merely has a Task Checkout does **not** refuse.
   Reuse `delete_junction`-style rendezvous to race a Task creation against
   removal and prove the gate fences it.
7. **Failure recovery.** Failing config write → memory and disk unchanged;
   `PublishedNotDurable` → reported as such, memory follows file, durable
   confirmation on next start; crash simulated between lock and publish →
   Project active; after publish → archived. Restore failures (missing repo
   directory, unreadable board, deleted Workspace) leave the archive intact
   and, for the Workspace case, restore as `Standalone`.
8. **Delete still works.** `delete_project` on an archived Project erases only
   its data and the archive entry; existing deletion tests keep passing.
9. **Config compatibility.** Config without `archived_projects` loads;
   archived section round-trips; the downgrade guard from decision 5.

Frontend tests (`cargo test -p slashit-frontend`): confirm dialog copy, the
refusal list rendering, the Removed Projects list, and the re-add prompt.
A desktop acceptance journey: add → create a Task → remove → restart →
re-add the folder → the Task and its history are present.

## Risks

- Anything that infers "Project absent from `projects`" as "orphan" will
  mistreat archived Projects. Needs an audit of `app_core` startup,
  `storage_accounting`, and board diagnostics before implementation.
- `agent_config.api_key` stays in `config.toml` for archived Projects; this
  matches present secrecy (0600) but means "remove" does not remove a secret.
- Disk is not reclaimed by removal; the UI must say so.

## Open decisions for the owner

1. **Where the archive lives:** a section of `config.toml` (proposed; atomic
   with the move) versus a separate `archived_projects.toml`. A separate file
   isolates a downgrade risk but loses single-transaction atomicity.
2. **Default on re-adding a known folder:** auto-restore versus always ask.
   Proposed: always ask, restore preselected.
3. **Strictness of the fingerprint:** is the root-commit match enough to offer
   a moved-folder restore, or should only the exact path restore?
4. **Archived Workspace members:** should a Workspace with archived members be
   undeletable (proposed), or should archiving detach to `Standalone`?
5. **Downgrade protection** for `archived_projects`: refuse to save on older
   builds (needs a marker), or document manual recovery as the Task board
   section does.
6. **Queued / In Progress Tasks:** refuse (proposed), or let removal move them
   back to Backlog on the person's confirmation.
7. **Secrets:** wipe the Project's `agent_config.api_key` from the archived
   record (requires re-entering on restore) or keep it.
8. **Retention:** archive forever (proposed) or offer an optional "erase after N
   days" — which would reintroduce an automatic destructive path, so likely no.
9. **Existing duplicate registrations of one folder:** report only
   (proposed) or offer a guided merge.
10. **CLI exposure:** read-only listing (proposed) or no exposure.
