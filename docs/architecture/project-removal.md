# Non-destructive Project removal

Status: **design proposal, not adopted.** Nothing here is implemented and no
decision in it is made; the owner decisions are listed at the end and the text
marks where the design depends on them. Statements about current behavior cite
the code they were read from (paths relative to `src-tauri/src`). Everything
else is marked **proposed**. Vocabulary follows
[product-model.md](product-model.md); storage locations follow
[state-locations.md](state-locations.md).

## Problem

The only way to take a Project out of the sidebar today is `delete_project`
(`commands/project.rs`). It is destructive: after refusing on live work it
removes the Project from `config.toml`, records cleanup, deletes the Project's
Task board files and its primary Conversation (`delete_project_committed`,
`commands/project.rs:283`; Task files via `delete_task_files`, Conversation via
`delete_project_conversation`, both driven from `app_core.rs:243-340`). Tasks,
their Activity and Human Review history live in the Task records, so they go
with the board.

There is nothing between "keep it forever" and "erase its history". This
proposal describes a third action, **Remove from SlashIt** (archive), that only
changes whether a Project is *active*. `delete_project` remains the separate
erase.

## Current facts the design rests on

Each fact below was read in the cited code.

- **Registry.** Projects and Repositories are records in `config.toml`
  (`AppConfig.projects`, `AppConfig.repositories`). A Project points at a
  Repository by `repository_id`; the path is `Repository.local_path`
  (`config/storage.rs` `state_dir_for`, ~1048).
- **No path comparison at registration.** `create_repository` validates the
  directory and mints `Uuid::new_v4()` (`commands/repository.rs:164`);
  `create_project` also mints a fresh id (`commands/project.rs:96`). Neither
  was seen comparing paths, so the same folder can be registered twice as
  independent Projects. I did not find a duplicate check; the proposal treats
  absence of one as the current behavior.
- **The project key is not stored.** It is re-derived from the repository path
  on every route refresh: `refresh_routes` calls `state_dir_for`, which does
  `ProjectKey::for_path(root).key` (`config/storage.rs:1033-1059`), and
  `app_core.rs` test setup derives it the same way. Board location is
  `paths.project_state_dir(&key, project.id, root, project.state_location)`
  (`config/paths.rs:438-450`): for the external location the key is part of the
  directory, for the in-project location the path is the repository itself.
  Consequence: **changing a Repository's `local_path` changes where an external
  board is looked for.** `update_project`'s own comment says a `repository_id`
  change "re-derives the Project's routes" (`commands/project.rs:707`).
- **Two existing documents say the opposite and are wrong.**
  `config/paths.rs:~161-166` (the `ProjectKey` doc) and
  `docs/architecture/state-locations.md:~153-158` state that the key is
  stored with the Project and travels with it. The code does not do that. This
  proposal does not rely on those claims. Correcting them is a separate
  documentation change, to be made whichever way the key decision below goes.
- **Board loading follows the active config.** `task_files()` builds its list
  from the routes of the Projects in the config, then also scans the legacy
  tasks directory for any `*.toml` not already claimed, deriving an owner from
  the file stem (`config/storage.rs:1510-1560`). So a Project that is not in
  `projects` has its routed board not loaded, but a board still in the legacy
  directory would be picked up (see Risks).
- **Cleanup is driven by pending records.** At startup, Conversation and Task
  cleanup run only for ids named in recorded pending-deletion files; for each
  such id, a Project still in config means the deletion never committed and
  the record is dropped, an absent one authorizes the cleanup
  (`app_core.rs:243-340`). Merely being absent from `projects` does not
  trigger any deletion.
- **Deletion's refusals.** `delete_project_committed` refuses on live work
  (`live_work_refusal`: running agent, deliveries in flight), a live Project
  Conversation Run (`commands/project.rs:~195-260`), and per Task
  `project_task_refusal` (`commands/project.rs:689`) wrapping
  `lifecycle::project_deletion_refusal` (`lifecycle.rs:1360-1392`): a pending
  local integration, an unfinished restack, `cleanup_in_flight`, **and a Task
  that still has a checkout (`worktree_path` set)** (`lifecycle.rs:1385-1391`).
  It also refuses when the Task records cannot be read
  (`commands/project.rs:~315`).
- **Locking template.** Conversation mutation lock, then the Project lifecycle
  gate in write mode, then every Task lifecycle lease, then the board write
  guard (`lock_project_conversation_for_deletion`, `project_gate.write`,
  `hold_project_task_leases`; `lifecycle.rs:~1340-1400`).
- **Task Checkouts** live under `<data_dir>/worktrees/<project-key>/`
  (`config/paths.rs:456-470`) and are listed by directory in storage
  accounting (`storage_accounting/inventory.rs:345-372`), with recorded owners
  taken from Task records.
- **Orphan tooling needs an active Project.** `scan_orphans` and the reclaim
  commands start from `projects.get(&project_id)` and answer
  `Project {id} not found` otherwise (`commands/orphans.rs:15-19`);
  `owners_in_repository` attributes a Task's checkout and branch to a
  repository only through an active Project and its repository
  (`worktree/orphans.rs:90-115`).
- **Workspace membership** is stored on the Project
  (`domain/project.rs`, `ProjectScope`), and `delete_workspace` refuses while
  active Projects reference it (`commands/workspace.rs:~93-110`, reading the
  projects map).
- **Secrets file mode is inconsistent in the docs.** `config/paths.rs:21` lists
  `AgentConfig::api_key` under "Secrets … `config_dir`, mode 0600", yet the
  key lives in `config.toml`, while `paths.rs:~320` says `credentials.toml` is
  the file "locked to 0600 independently". The only test I found asserts that
  `config.toml` has no group/world bits (`config/storage.rs:~4048-4058`), not
  that the mode is exactly 0600. I did not verify the mode on the write path.

## Design

### 1. Archived Project record (proposed)

Add a section to the registry, `archived_projects`, keyed by Project id:

```text
ArchivedProject {
  project: Project,       // the full record as it was
  repository: Repository, // the record it pointed at
  archived_at: DateTime<Utc>,
  board_files: Vec<PathBuf>, // captured routes, as delete_project captures them
  checkout_refs: Vec<..>,    // see section 5
}
```

`board_files` uses the same capture `delete_project` already makes
(`storage.tasks_board_files`, `config/storage.rs:1717`): once the Project is
out of the active set the config can no longer route to its board, so the
paths are recorded at removal. They are evidence of where the board was, not a
key (the key is not stored; see the key decision below).

Removal **moves** the record: out of `projects`, into `archived_projects`, one
config transaction, so a crash leaves it in exactly one section. The
Repository record moves only if no other active Project references it.

Removal touches no file, Git ref, worktree, Conversation, credential or board.
That invariant is testable.

### 2. What is preserved

| Data | Location | After removal |
|---|---|---|
| Tasks, Activity, Human Review decisions, delivery facts | board file | untouched on disk; not loaded into the active board |
| Primary Conversation | `conversations_dir()` file (`primary_conversation_file`, `config/paths.rs:377`) | untouched |
| Task branches, Git recovery refs | the repository | untouched |
| Project settings, base, scope | `config.toml` | in `archived_projects` |
| Pending PR / integration facts | on the Tasks | untouched |
| `agent_config.api_key` | `config.toml` | kept in the archived record unless the owner decides otherwise (decision 2) |

Activity and Human Review are fields of the Task records; preserving the board
preserves them, with no separate store.

### 3. Project key and moved repositories: the key decision

Because the key is re-derived from the path (facts above), a design that
promises "re-adding restores the previous identity" must say what happens when
the path is not the old path. Two coherent options; **the owner chooses**
(decision 5), and this proposal adopts neither:

- **A. Store the key, with a migration.** Add the key to `Project` (or
  `Repository`), populated for every existing Project from the current path at
  upgrade time, and make `state_dir_for` read it. A moved repository can then
  be re-pointed and keep its board. Cost: a config migration that must run
  before any route refresh, a downgrade story (an older build re-derives and
  silently orphans), and correcting the two documents above to match.
- **B. Forbid re-pointing a moved path.** Restore only when the repository's
  canonical path equals the archived `local_path` (so the re-derived key and
  the in-project location both resolve to the existing board). A moved
  repository is added as a new Project; the archive stays restorable by moving
  the folder back. Cost: no migration, but a moved repository cannot recover
  its history.

Under either option, the archived `board_files` let restore verify before
activating that the board it expects is present and parses.

### 4. Re-adding the same repository, and identity conflicts

Definition (**proposed**): two registrations name the same repository when
their canonical absolute paths are equal (symlinks resolved, trailing
separators removed; case-folding only on case-insensitive filesystems).
Whether a root-commit fingerprint is also used to *suggest* a moved folder is
decision 4.

Rules at registration (`create_repository`, then `create_project`):

- Path equals an **active** Project's repository: refuse, naming that Project.
  This also closes today's silent duplicate registration for new Projects.
  Duplicates already on disk are reported and left alone.
- Path equals an **archived** Project's repository: do not mint a new
  Project. Offer **Restore** (preselected) or, explicitly, **Add as new
  Project**. Restore re-activates the same Project id, hence the same board
  directory (`project_state_dir` ends in the Project id), the same
  Conversation file and the same Tasks.
- A different repository at an archived path (a different root commit, when
  that check exists): Restore is refused with an explanation; Add as new
  remains.
- Uniqueness: a Project id is unique across `projects ∪ archived_projects`.
  Restore, add, and archive assert this inside the single config transaction
  under the Project gate, so two concurrent restores, or a CLI and the GUI,
  cannot both succeed and no id is ever in both sections.
- Adding as new at an archived path does not adopt or delete the archive.
  Two Projects on one folder cannot share a board because the state directory
  ends in the Project id (`config/paths.rs:438-450`, test
  `project_state_dirs_do_not_collide_for_one_repository`).

Restore validates before activating, and a failed validation leaves the
archive unchanged:

- the directory exists and is a Git repository (the registration check);
- the board parses under the current contract; an unreadable board is reported
  by path and the Project is not activated against it
  (`config/storage.rs` unloaded-board guard);
- the Workspace named by `scope` still exists. If not, restore as
  `Standalone` and say so (see decision 1 for whether that situation should be
  possible at all).

### 5. Ownership of archived Task Checkouts and branches

Today a Task Checkout and its branch are owned by the Task that records them,
through an *active* Project. After removal nothing would own them: orphan scan
answers `Project not found` for an absent Project, so nothing can reclaim
them, `owners_in_repository` cannot attribute them, and storage accounting
still counts the directories (they exist under `worktrees/`), so removal would
neither free space nor explain it. The proposal therefore defines ownership
explicitly:

1. **The archived record owns them.** A Task Checkout or branch referenced by
   an archived Project's board belongs to that archived Project until it is
   restored or permanently deleted.
2. **Refuse removal while any Task has a Task Checkout.** This follows
   `delete_project`, which refuses with "still has a checkout"
   (`lifecycle.rs:1385-1391`, wrapped at `commands/project.rs:689`). The
   reason there is that dropping the Task records would strand the checkout;
   that applies equally here, because an archived Project's checkout has no
   active owner and no reclaim path. The person deletes or finishes those
   Tasks (which removes the checkout and keeps Git recovery refs) first.
   Branches and recovery refs of finished Tasks remain in the repository and
   belong to the archive.
3. **Orphan tools refuse archived Projects explicitly.** `scan_orphans` and
   the reclaim commands should answer "this Project is removed from SlashIt"
   for an archived id, never treat its refs as unowned, and `reclaim_*` must
   not run for them. Required change to `commands/orphans.rs:15-19` and
   `worktree/orphans.rs:90-115`.
4. **Accounting reports them as "archived Projects".** The inventory needs the
   archived record's `checkout_refs` (paths and branch names captured at
   removal from the board) as owners, so those directories are attributed to
   the archive instead of appearing unowned
   (`storage_accounting/inventory.rs:345-372`). Removal does not free space,
   and the UI says so.

Whether refusing on any recorded checkout is too strict (a Done Task may keep
`worktree_path`, `lifecycle.rs:~1671`) is for the owner; the alternative is a
reclaim-then-remove flow. It is listed under decision 6.

### 6. Refusals

Same locks, same order as deletion, taken before anything changes. Removal
refuses when:

- an agent runs for a Task of the Project, or a delivery is in flight
  (`live_work_refusal`);
- the Project Conversation has a live Run (`project_run_is_live`);
- any Task trips `lifecycle::project_deletion_refusal`: pending integration,
  unfinished restack, `cleanup_in_flight`, or a Task Checkout (section 5);
- a Task is queued or in progress, so the queue cannot pick up or lose track
  of it (whether to refuse or return it to Backlog: decision 7);
- a pending deletion record exists for the id
  (`has_pending_tasks_deletion_record`);
- the Task records cannot be read (same reasoning as
  `commands/project.rs:~315-320`);
- the config cannot be written. A write visible but not confirmed durable
  (`PublishedNotDurable`) is reported as "removed, durability unconfirmed";
  the existing `reconfirm_config_durable` step settles it on next start
  (`app_core.rs:~232-245`).

Archiving an archived id is a no-op ("already removed"). Restore refuses
while a deletion of that id is pending.

### 7. Failure and restart behavior

One config transaction, so no multi-file recovery protocol:

- failure before publish: config and memory unchanged, error returned;
- published but not durable: reported as such; next start reconfirms;
- crash after publish: next start reads `archived_projects`, loads no routed
  board, runs no cleanup (no pending record exists, `app_core.rs:243-340`);
- crash before publish: Project still active.

A build that does not know `archived_projects` would drop it on its next config
save unless unknown keys are preserved or the file is version-marked. Not
verified here; decision 3 covers the protection.

### 8. Surfaces

- Backend commands `archive_project`, `restore_project`,
  `list_archived_projects`, GUI-only through `GuiMutation` like
  `delete_project`. The CLI/IPC channel gets read-only listing at most.
- `delete_project` keeps its behavior and, given an archived id, erases the
  archived data and the archive entry.

### 9. UI explanation

Project settings get two separate actions.

**Remove from SlashIt** (default, not danger-styled):

> Remove *<name>* from SlashIt? It will disappear from your Projects and stop
> running. SlashIt keeps its Tasks, history, reviews and Conversation, and
> leaves your repository and branches exactly as they are. It does not free
> disk space and does not touch GitHub. Add the same folder again to restore
> it.

On refusal, a dialog lists each blocker with its Task and the next step
("Delete or finish *Fix login*, which still has a Task Checkout"; "Resume or
discard the unfinished integration on *X*"; "Stop the running agent on *Y*").

**Delete permanently…** (danger style): states that Task records, Activity,
reviews and the Conversation are erased, repository files, branches and
recovery refs are not, and requires typing the Project name.

A "Removed Projects" list shows name, path, removal date, Task count, and
Restore / Delete permanently. Adding a matching folder shows: "This folder was
removed from SlashIt on <date>. Restore it with its <n> Tasks, or add it as a
new Project." "Workspace" means only the Product Workspace; "Task Checkout" is
used per the vocabulary.

## Test plan

Backend tests beside `commands/project.rs`, with a temporary `Storage`, as the
existing deletion tests do:

1. **Removal preserves.** Seed Tasks (Activity, Human Review decisions), a
   Conversation, a branch. Remove. Board and Conversation files are
   byte-identical, the config lists the Project only in `archived_projects`,
   nothing deleted, no Git ref or worktree changed.
2. **Active surfaces forget it.** After removal and a fresh `build_state`: no
   queue, attention, CLI listing or PR poll entry; no pending cleanup is
   created; the board file is still present. Include a Project whose board is
   still in the legacy tasks directory and prove it is not loaded
   (`config/storage.rs:1545-1560`).
3. **Restart.** Remove, rebuild, still archived, files intact; restore,
   rebuild: same id, Tasks, Activity, Human Review history, Conversation.
4. **Re-addition.** Same path: restore offered, same id, same board directory,
   no second Project. Active duplicate: refused. Different repository at the
   path: restore refused, add-as-new creates a fresh id and leaves the archive
   alone. Moved path: behaves per the key decision (A: re-pointed board is
   found; B: restore refused, folder moved back restores).
5. **Key behavior.** Assert today's re-derivation (changing `local_path`
   changes the external board route) so the key decision is pinned by a test,
   and, under option A, a migration test: existing Projects get a key equal to
   the one derived now, and a second load does not change it.
6. **Concurrency.** Two simultaneous restores, and a restore racing a create at
   one path: exactly one wins; no id in both sections. A Task creation raced
   against removal is fenced by the gate (reuse the `delete_junction`
   rendezvous).
7. **Refusals.** One test per condition in section 6, including a Task with a
   Task Checkout; each asserts config and files unchanged.
8. **Ownership.** `scan_orphans` and `reclaim_*` on an archived id refuse
   without listing its refs; storage accounting attributes the archived
   Project's retained directories to "archived Projects", not to unowned.
9. **Failure recovery.** Failing config write: memory and disk unchanged.
   `PublishedNotDurable`: reported, reconfirmed at next start. Crash between
   lock and publish: still active; after publish: archived. Restore with a
   missing directory, unreadable board or deleted Workspace leaves the archive
   intact.
10. **Delete still works.** `delete_project` on an archived Project erases only
    its data and the entry; existing deletion tests pass unchanged.
11. **Config compatibility.** A config without `archived_projects` loads; the
    section round-trips; the downgrade protection chosen in decision 3.

Frontend (`cargo test -p slashit-frontend`): confirmation copy, refusal list,
Removed Projects list, re-add prompt. One desktop acceptance journey: add,
create a Task, remove, restart, re-add the folder, the Task and history are
present.

## Risks

- The legacy tasks directory scan loads any unclaimed `*.toml`
  (`config/storage.rs:1545-1560`); archived ids must be added to the claimed
  set or such a board would load as if active.
- Other code that reasons "Project absent from `projects`" (attention,
  accounting, orphan tools, board diagnostics, `storage_accounting`) must be
  audited for archived Projects before implementation. Startup cleanup itself
  is not one of them, per the pending-record behavior above.
- An archived record keeps `agent_config.api_key` in `config.toml`; removal
  does not remove a secret. The file mode is unverified (facts above): the
  design requires that the archived section never be written with weaker
  permissions than the active registry, and requires settling and testing the
  exact mode (and fixing the `paths.rs:21` doc) before implementation.

## Open decisions for the owner (none adopted)

1. **Archived Workspace membership.** Keep `ProjectScope::InWorkspace` on the
   archived record and make `delete_workspace` count archived members, or
   detach to `Standalone` at archive time, or restore as `Standalone` when the
   Workspace is gone. Currently only active Projects block
   `delete_workspace`.
2. **Whether archive wipes `api_key`.** Wiping removes a secret from
   `config.toml` but is **irreversible**: restore would require re-entering
   it. Keeping it leaves a secret on disk for a Project the person believes
   is gone.
3. **Archive location.** A section of `config.toml` (atomic with the move; a
   downgrade can drop it) or a separate `archived_projects.toml` (isolated,
   but the move is no longer one transaction), and the downgrade protection
   for either.
4. **Root-commit fingerprint.** Whether a root commit (and remote) is used to
   detect "same repository" and to refuse a restore onto a different
   repository, or only the canonical path is used.
5. **Re-registration of a moved repository.** Option A (stored key with a
   migration) or option B (forbid re-pointing) from section 3, including how
   the incorrect key statements in `config/paths.rs` and `state-locations.md`
   are corrected.
6. **Task Checkout refusal strictness.** Refuse on any recorded Task Checkout
   (the stated position, following `delete_project`) or add a
   reclaim-then-remove flow for finished Tasks.
7. **Queued / in-progress Tasks.** Refuse, or move them to Backlog on
   confirmation.
8. **Default on re-adding a known folder.** Always ask with Restore
   preselected, or auto-restore.
9. **Retention.** Archive forever, or an opt-in expiry (which reintroduces an
   automatic destructive path).
10. **Existing duplicate registrations.** Report only, or a guided merge.
11. **CLI exposure.** Read-only listing, or none.
