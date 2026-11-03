# Logical hide for Projects

**Status: Proposed. Not implemented. Not adopted. Section 12 marks which decisions are settled for phase 1 and which remain open.**

This document replaces the earlier version of this proposal, which described a true archive. It describes logical hide only. Permanent deletion and moved-folder recovery are deferred (section 9). No code, configuration, or test is changed by this document.

## Labels

- **VERIFIED CURRENT** means the claim was read on main after PR #299 was merged. Each one carries a repository-relative file and line, or the file and function name when only the function was checked. Line numbers are as read on that commit.
- **PROPOSED** means the behavior would be added by this design. It has not been built or tested.
- Any sentence that describes hide, restore, or admission behavior is PROPOSED unless it carries VERIFIED CURRENT. Such sentences use "would". Nothing here says that hiding is in effect.

Vocabulary follows `docs/architecture/product-model.md`. Storage locations follow `docs/architecture/state-locations.md`.

## 1. Problem and user goal

**VERIFIED CURRENT.** The only way to remove a Project from the sidebar is `delete_project` (`src-tauri/src/commands/project.rs:173-238`). After refusing on live work, it removes the Project from `config.toml`, deletes the Project's Task board file and primary Conversation (`delete_project_committed`, `project.rs:283`). Tasks, Activity, and Human Review records live in the board file, so they are erased with it. No reversible option exists.

**Goal (agreed direction).** Remove a Project from the active SlashIt workspace without deleting the Project, its repository, files, Tasks, Conversations, history, configuration, or identity, and allow it to be restored later. This is not permanent deletion and not an archive-management framework.

**PROPOSED.** Add a reversible state, logical hide. The Project record, its UUID, its repository association, its Task history, its Conversation, its review records, its configuration, and its Workspace membership would all stay in place. A hidden Project would be inactive: ineligible for new work and absent from ordinary navigation. Enforcement would be backend admission checks (section 4), not frontend filtering.

## 2. Current facts the design rests on

Each item is VERIFIED CURRENT on main after PR #299 was merged, unless marked otherwise.

### 2.1 Records and configuration

- `Project` has the fields `id`, `name`, `repository_id: Option<Uuid>` (`src-tauri/src/domain/project.rs:80`), `scope`, `state_location`, `base` (`domain/project.rs:105`), `agent_type`, `agent_config`, `created_at`, `updated_at` (struct at `domain/project.rs:77-110`). `Project::repository_path` returns `None` for a repository-less or dangling Project (`domain/project.rs:121-125`).
- `AgentConfig` holds `api_key: Option<String>` (`domain/project.rs:138-144`, field at line 144).
- Neither `Project` (`domain/project.rs`) nor `AppConfig` (`src-tauri/src/config/storage.rs:199`) carries `deny_unknown_fields`. The attribute at `storage.rs:75` is on the board file type `ProjectTasksFile`.
- `AppConfig.projects` and `AppConfig.repositories` are keyed maps (`storage.rs:201`, `:203`).
- `Repository` has `id`, `local_path`, `remote_url: Option<String>`, `remote_type`, `created_at` (`src-tauri/src/domain/repository.rs:6-12`).
- `update_config` reads the file strictly, runs a mutation, and writes (`storage.rs:929-940`). `write_config` serializes the whole `AppConfig` (`storage.rs:1001-1005`) and replaces the file atomically with `Permissions::OwnerOnly` (`storage.rs:1356-1361`, permission argument at line 1360).
- `try_persist_projects` rebuilds the entire `projects` table from the in-memory map it is given, through `update_config` (`src-tauri/src/commands/project.rs:33-48`). **The in-memory map is authoritative for the whole table whenever a Project writer persists.**
- Every Project writer clones the map under the in-memory `projects` write lock, persists the clone, and adopts it on success or durable-unconfirmed success. Verified for create (`commands/project.rs:139-151`, lock at 141), update (`project.rs:717-750`, lock and clone at 720-721, adoption at 743), and workspace attach (`project.rs:783-812`, lock at 790, clone at 795, persist at 808). `adopt_published` is the adoption helper.
- `ProjectScope` is `Standalone` or `InWorkspace { workspace_id }` (`domain/project.rs:14-18`).

### 2.2 Routes, board location, and StateLocation

- `refresh_routes` runs after every config write and iterates every Project in `config.projects` (`storage.rs:1033-1050`).
- `state_dir_for` derives the project key from the Repository's `local_path` on every refresh (`storage.rs:1054-1058`). The key is not stored in `config.toml`.
- `task_files` builds its list from the routes, then scans the legacy tasks directory for unclaimed `*.toml` files (`storage.rs:1510-1562`, legacy sweep at `1545-1559`).
- `project_state_dir` joins the Project id under the external key directory or the in-project location (`src-tauri/src/config/paths.rs:438-450`). The primary Conversation file is `primary_conversation_file` (`paths.rs:377`). Task Checkouts live under `worktrees_root(key)` (`paths.rs:464-470`).
- `StateLocation` has `External` (default), `InProject`, and `Auto` (`paths.rs:58-67`). `Auto` resolves to `InProject` when a valid `.slashit/` exists and to `External` otherwise, so **its result depends on repository contents, which can change while a Project is hidden.**
- **Documentation mismatch.** `config/paths.rs:161-165` says the workspace registry stores the key alongside the path. `docs/architecture/state-locations.md:153-156` says the stored key travels with the Project. The code does neither: it re-derives the key (`storage.rs:1054-1058`). This is separate documentation work.

### 2.3 Deletion, refusals, and locks

- `delete_project` acquires, in order: the Project Conversation lock (`project.rs:195-206` via `lock_project_conversation_for_deletion` at `project.rs:240-256`), the Project lifecycle gate in write mode (`project.rs:216`), and every Task lifecycle lease of the Project (`project.rs:223-228`, `lifecycle::hold_project_task_leases`). The function comment states that the order is "Conversation, gate, task leases, shared board".
- A live Project Conversation Run refuses deletion (`project.rs:247-253`).
- `live_work_refusal` refuses on a running agent (`is_task_running`) or a pull request being opened (`commands/project.rs:660-685`; the in-flight check uses `DeliveriesInFlight`).
- `project_task_refusal` wraps `lifecycle::project_deletion_refusal` (`project.rs:689-695`; `lifecycle.rs:1360-1393`). That function refuses on a pending `local_integration`, a pending republish, `cleanup_in_flight`, and any `worktree_path` (`lifecycle.rs:1363-1391`).
- `update_project` takes the gate in read mode (`project.rs:698-715`, gate at line 710). It has no `api_key` parameter (`project.rs:698-705`).
- **Credential clearing.** `update_project_committed` replaces the whole `agent_config` with `get_default_agent_config(&agent_type)` when `agent_type` is supplied (`project.rs:737`). That default has `api_key: None` (`project.rs:69-88`). **An ordinary agent-type change therefore clears a configured `api_key` today.** See section 7.

### 2.4 Admission points that exist today

- **Conversation.** `project_after_lock` re-reads the Project under the Conversation lock and answers "Project not found" (`src-tauri/src/commands/conversation.rs:197-207`). Callers: `open_project_conversation` (fn at `conversation.rs:210`, call at 219), `get_project_conversation` (call at 258), `retry_project_conversation_continuation` (call at 354), `send_project_message` (call at 401), `retry_project_message` (call at 652), and `act_on_project_conversation` (call at 811).
- **Task creation.** `create` delegates to `create_checked` (`src-tauri/src/lifecycle.rs:586-598`). `create_checked` takes the gate in read mode (`lifecycle.rs:613`), takes the Task write lock, and checks that the Project exists under that lock (`lifecycle.rs:625-629`) before staging and publishing. `create_batch` makes the same check (`lifecycle.rs:648-668`). Callers: `commands/task.rs:133` (`create_checked`), `commands/jira.rs:232`, `commands/github.rs:281` and `:406` (`create_batch`), and the IPC create handler (`ipc/handlers.rs`, which calls `lifecycle::create`). Conversation-driven creation calls `lifecycle::create` at `src-tauri/src/conversation_actions.rs:437` and `:1166`.
- **Queue entry.** `queue_move_refusal` (`lifecycle.rs:869`) is called from `commands/task.rs:231`, `:287`, and `:940`, from `ipc/handlers.rs:305` and `:376`, and from `commands/human_review.rs:611` and `:625`. Queue entry functions are `enqueue_leased` (`lifecycle.rs:913`), `enqueue_current` (`lifecycle.rs:982`), and `enqueue_current_refusing_human_review` (`lifecycle.rs:997`, body via `enqueue_current_held` at 1011). Their bodies were not read in full.
- **Promotion.** `select_promotable` (`src-tauri/src/queue/manager.rs:55-80`) takes the task map and a `lease_admits` closure and has no Project input. `promote_queued` (`src-tauri/src/queue/executor.rs:2360-2390`) builds the selection with `start_lease_admits(&self.storage, task)` (`executor.rs:670`), which takes storage and a Task, not a Project.
- **Background loop.** `polling_loop` (`executor.rs:2058`) drives `check_and_execute` (`executor.rs:2574`), which promotes and starts work.
- **Execution.** `start_new_attempt` (`executor.rs:5141`); `start_execution_under_lease` (`executor.rs:5345`, `is_task_running` check at 5356); `acquire_task_worktree` (`executor.rs:3847`); `execute_task` (`executor.rs:6201`).
- **Worktree creation.** `create_worktree` (`src-tauri/src/commands/worktree.rs:6-13`) calls `create_worktree_inner` (`worktree.rs:16`), which takes the Task lifecycle lease (line 22), the start guard (line 26), checks `cleanup_in_flight`, and resolves the Project's repository (Project lookup at line 51).
- **Project files.** `project_root_for` (`src-tauri/src/commands/file.rs`, Project lookup at line 36) resolves a Project's folder for file commands, and refuses paths outside it.
- **PR helpers.** `resolve_repository_dir` (`src-tauri/src/commands/pr.rs:118-137`, Project lookup at line 130) resolves a Task's repository directory for PR read helpers. Its own comment says these helpers "write nothing, spawn no agent".
- **Human Review actions** (`src-tauri/src/commands/human_review.rs`). `approve_task` (line 105), `create_approved_task_pr` (119), `complete_task_locally` (136), `request_task_changes` (153), `approve_task_again` (257), `approve_again` (267), `retry_delivery` (401), `complete_locally` (441), `request_changes` (583), and `deliver` (671). `approve_again` refuses once the Task has a PR (`pr_url` set or a PR external reference, `human_review.rs:267-283`), which is why a PR-linked Task's approval cannot be renewed through this path.
- **Attention.** `get_attention_summary` (`src-tauri/src/commands/attention.rs:60-67`) summarizes the Tasks of every Project and reads `DeliveriesInFlight`. It does not filter by Project state.
- **PR status.** `apply_polled_pr_state` (`executor.rs:3258`) writes GitHub PR state onto Tasks. The user-triggered refresh path in `commands/pr.rs` (about lines 6546-6640) applies polled states the same way.
- **Startup.** `build_state` (`src-tauri/src/app_core.rs:86`) reconfirms config durability only when pending Project deletions are owed (`app_core.rs:236-257`). The Task reconciliation loop runs over every Task (`app_core.rs:403-418`). The comment at lines 432-435 says that adopting a checkout that is mid-cleanup would hand "a half-removed directory straight back to the executor", so reconciliation is gated on `cleanup_in_flight`.
- **Orphans.** `context` in `src-tauri/src/commands/orphans.rs:12-22` looks up the Project and answers "Project {id} not found" when it is absent. `owners_in_repository` (`src-tauri/src/worktree/orphans.rs:88-138`) builds the owner set from **every** Task: it adds each Task's `worktree_path` to `checkout_paths` unconditionally, and adds its `task_id` and `branch_name` when the Task's Project repository has the same identity as the repository being scanned, or when that identity cannot be determined (`worktree/orphans.rs:120-132`).
- **Accounting.** `worktrees` in `src-tauri/src/storage_accounting/inventory.rs:345-374` attributes each checkout to the Task records that record it.

### 2.5 Unresolved work: what the code actually tracks

- `TaskStatus` is `Backlog`, `Queue`, `InProgress`, `AiReview`, `HumanReview`, `Done`, `PrCreated`, `Error` (`src-tauri/src/domain/task.rs:1902-1911`).
- **Durable markers of unfinished work on a Task** (VERIFIED CURRENT):
  - `local_integration` pending (`domain/task.rs:836`, `lifecycle.rs:1363`).
  - `pending_republish` present (`domain/task.rs:870`; `PendingRepublish` at `task.rs:1315`).
  - `cleanup_in_flight` (`lifecycle.rs:1375`).
  - `worktree_path` present (`lifecycle.rs:1385`).
  - `new_attempt` present (`domain/task.rs:760`, `settled_checkout_refusal`).
  - `worker_handoff` stage `Started`, `CommittedAwaitingReview`, or `RecoveryRequired`, or `ResolvedToAiReview` that was not superseded (`domain/task.rs:760-790`).
- **In-memory markers** (process-local, not durable): `DeliveriesInFlight`, a counted set of Tasks with a PR being opened (`commands/human_review.rs:34`, held as `state.task.deliveries` at `commands/task.rs:44`); and `is_task_running` on the executor.
- **`PrCreated` is not an owner of work.** It records that a pull request exists. A Task in `PrCreated` with no checkout, no pending record, and no in-flight delivery has no active owner.
- **`HumanReview` is a decision owner.** It waits on a person's approve or request-changes decision. Those actions are refused for hidden Projects (section 4), so such a Task would be stuck while hidden.

### 2.6 Workspace, IPC, and secrets

- `projects_referencing` counts every Project whose `scope` names the Workspace, regardless of any other state (`src-tauri/src/commands/workspace.rs:77-83`). `delete_workspace` refuses while any such Project exists (`workspace.rs:86-115`, refusal at 104-111).
- `detach_project_from_workspace` exists (`commands/project.rs:814-850`).
- `list_projects` returns clones of every Project, including `agent_config.api_key` (`commands/project.rs:155-160`). `get_project` returns the clone for one id (`project.rs:163-170`).
- The IPC list handler returns only `id`, `name`, and the repository `path` (`src-tauri/src/ipc/handlers.rs`, `handle_list_projects`, lines 112-125). The IPC channel does not expose `api_key` today.
- `config/paths.rs:21` says secrets, including `AgentConfig::api_key`, live in `config_dir` with mode 0600.

### 2.7 Released builds

- Tags `v0.0.2` and `v0.0.3` exist. At both tags, `domain/project.rs` has `Project` and `AgentConfig` with no `deny_unknown_fields` (checked by grep). `storage.rs` has `AppConfig` with no deny attribute on the struct.
- `domain/project.rs` on main contains no `flatten` attribute, and neither does `storage.rs`. Unknown keys inside a Project table are therefore dropped on re-serialization by any build that does not know them.

### 2.8 Existing tests that can serve as fixtures

Verified by name at these lines on main:

- `commands/project.rs:1204` (`create_project_leaves_memory_unchanged_when_persistence_fails`), `:1223` (delete leaves memory and board unchanged on persistence failure), `:1595` (`live_project_conversation_refuses_deletion_before_persistence`), `:2107`, `:2131`, `:2155`, `:2174`, `:2193` (refusals for pending integration, unexpected status, pending restack, active agent run, and delivery in flight), `:2950` (Task creation after deletion is refused), `:3002` (concurrent Task creation and deletion).
- Workspace tests: `commands/workspace.rs:154-181` (`projects_referencing` tests).
- `commands/worktree.rs:183` (test helper `git`).
- `queue/manager.rs:293-400` (`select_promotable` tests).

No test exists today for hide, restore, the hidden marker, or admission by hidden state.

## 3. Proposal: logical hide

### 3.1 Stored state (PROPOSED)

One optional field is added to `Project`, following the pattern of the existing optional `base` field (`domain/project.rs:105`):

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_at: Option<chrono::DateTime<chrono::Utc>>,

A Project is hidden if and only if `hidden_at` is `Some`. The Project stays in `config.toml` under `projects`. There is no new section, no new file, and no record move.

### 3.2 Single-field invariant (PROPOSED)

Hide and restore would each change exactly one field in `config.toml`: `projects.<id>.hidden_at`. They would not change `updated_at`, and they would not touch any other file.

Two conditions make this hold, and both are part of the design:

- The write goes through `try_persist_projects`, which rewrites the whole `projects` table from the in-memory map (section 2.1). The in-memory map must therefore equal the on-disk table when hide runs. Startup loads from disk, and every Project writer adopts what it persists, so the two agree unless a previous write was left unconfirmed. An unconfirmed write is reconfirmed before hide proceeds (section 5.5).
- The invariant is tested by parsing `config.toml` before and after, and comparing the two trees. The only difference allowed is the `hidden_at` key of the target Project. A byte-level comparison of every other file in the data and config directories, and of the repository's refs and worktree list, would also be required to be unchanged.

### 3.3 What is preserved (PROPOSED)

| Data | Location | Under hide | Label |
|---|---|---|---|
| Project id, name, `repository_id`, `scope`, `state_location`, `base`, `agent_config` | `config.toml` | Unchanged except `hidden_at` | PROPOSED |
| Repository record | `config.toml` `repositories` | Unchanged | PROPOSED |
| Tasks, Activity, Human Review decisions, delivery facts | Routed board file (`paths.rs:438-450`) | Bytes unchanged | PROPOSED |
| Primary Conversation | `primary_conversation_file` (`paths.rs:377`) | Bytes unchanged | PROPOSED |
| Task Checkouts | `worktrees_root` (`paths.rs:464-470`) | Unchanged; hide refuses if any exists | PROPOSED |
| Task branches and Git recovery refs | The repository | Unchanged | PROPOSED |
| Workspace membership | `Project.scope` | Unchanged | PROPOSED |
| `api_key` | `config.toml` | Kept on disk; never returned for a hidden Project (section 7) | PROPOSED |

### 3.4 Hide preconditions (PROPOSED)

Hide would refuse before any write if any of the following holds. The check would be one function, for example `project_hide_refusal(project, tasks, running, deliveries)`, evaluated under the locks in section 5.1. Each refusal names the Task and says what must be resolved first.

**Project-level:**

1. The Project Conversation has a live Run (`project.rs:247-253`).
2. A pending deletion record exists for the Project (`pending_project_conversation_deletions` and `pending_project_tasks_deletions`, `app_core.rs:236-245`).
3. The Task records cannot be read.

**Task-level, for every Task of the Project:**

4. Status is `Queue`, `InProgress`, `AiReview`, or `HumanReview`. These are live, queued, or awaiting-decision states. The `HumanReview` clause is needed because the decision actions are refused for hidden Projects (section 4), so the Task would otherwise wait indefinitely.
5. `worktree_path` is present, whatever the status (`lifecycle.rs:1385`). This is rule D3.
6. Any durable unfinished-work marker from section 2.5 is present: `cleanup_in_flight`, a pending `local_integration`, a pending `pending_republish`, a `new_attempt`, or a `worker_handoff` that is not settled. The last clause is the same test that `settled_checkout_refusal` applies (`domain/task.rs:760-790`).
7. An agent is running for the Task (`is_task_running`), or a pull request is being opened (`DeliveriesInFlight` contains the Task). These are the in-memory checks in `live_work_refusal` (`project.rs:660-685`).

**Not blocking** (D3: do not forbid hiding merely because historical records exist):

- `Backlog` Tasks with no checkout and no marker in item 6. These are inert: nothing promotes them (section 4, rows 4-5), and their admission refuses while hidden.
- `Done` Tasks and `Error` Tasks that have no checkout and no marker in item 6.
- `PrCreated` Tasks that have no `worktree_path` (item 5) and no marker in item 6. The pull request is a GitHub-side object. SlashIt would stop refreshing its state until the Project is restored. **Recommendation (O6):** the hide confirmation would list the count of Tasks with open pull requests and say that their status will not update while the Project is hidden. It would not block.

A `PrCreated` Task that still holds a checkout is not in this list. Item 5 applies to it, so it blocks hide regardless of status. A `PrCreated` status alone does not make a Task safe to hide.

**Checkout release (open requirement).** Hide does not release, detach, delete, or reclaim a Task Checkout to make hiding possible. Any of those would change or destroy Task state that the owner must keep. As a result, phase 1 may refuse hide for some Projects until their checkouts are handled by a separate, explicit mechanism. Defining that mechanism is a lifecycle requirement, not a shortcut. The implementation must establish, before offering hide for such a Project:

- which existing or new release path, if any, is safe for a `PrCreated` Task;
- that no delivery is in flight, no marker in item 6 is present, and no run is live;
- what happens to the Task branch, the recovery refs, and the Task's Git state;
- how the user starts the release, and that it is a deliberate action with its own tests and review.

The hide refusal must name the Task and its checkout, and it must point to that resolution path. It must not suggest deleting the Task or the Project.

**Idempotence.** Hiding an already hidden Project would succeed without a write.

### 3.5 Restore (PROPOSED)

Restore would clear `hidden_at` in one write. It takes the Project id. **It does not take a folder.** A folder selection can never restore a Project.

Preconditions, evaluated under the locks in section 5.1:

- **Repository-less Project** (`repository_id` is `None`): no path check applies. Restore clears `hidden_at`. The Project's identity is its UUID, and no folder is invented (D4).
- **Project with a Repository:**
  1. The Repository record exists in `repositories` (`storage.rs:203`).
  2. Its `local_path` still exists and canonicalizes to the same path as stored. A moved folder fails this check.
  3. The repository identity check that the orphan scan uses (`repository_identity` in `worktree/orphans.rs`) reports the same repository, using `remote_url` when it is recorded. The exact call is an implementation detail to be confirmed. This is the "expected original identity" check of D4.
  4. For a `state_location` of `Auto`, the board file at the resolved route exists, and no conflicting second board exists (see section 2.2). If both a routed board and a legacy board exist with different contents, restore refuses with a message. It does not guess.
- If a precondition fails, restore is refused with a specific message, for example "The folder recorded for this Project is not available at <path>. Moved-folder recovery is not available in this version." The message does not suggest creating a new Project for that folder.

Other rules:

- Restore keeps the same Project id, Repository record, Tasks, Conversation, and Workspace membership. It never creates an id.
- Restore on a visible Project is a no-op, not an error.
- A Workspace named in `scope` cannot disappear while the Project is a member (`workspace.rs:104-111`), so no separate Workspace check is needed at restore.
- Root-commit fingerprinting is not part of this proposal.

### 3.6 Add with a folder that a hidden Project still uses (PROPOSED)

The Add flow calls `create_repository` first (`src/components/create_project_modal.rs:230`), then `create_project`. `create_repository_core` (`src-tauri/src/commands/repository.rs:149-170`) mints a new Repository id and performs no path comparison, so a re-added folder receives a new id and an id-based check cannot match. Today no duplicate-path refusal exists.

Under this proposal, `create_repository` would compare the canonicalized `local_path` against the `local_path` of every Repository referenced by a **hidden** Project, before any record is written. A match would be refused with "This folder belongs to the hidden Project <name>. Restore that Project instead." A folder used only by visible Projects keeps today's behavior, because shared Repository records are already allowed. This refusal is the add-flow form of D4. It is recommended as **O7**; the wording and the choice between restoring and adding remain open.

### 3.7 Backlog and inert Tasks (PROPOSED)

The question is whether inert Backlog Tasks may remain under a hidden Project. The answer under this design is **yes**, for Backlog Tasks with no checkout and no unfinished marker. Three independent controls keep them inert while hidden:

- No path moves them into Queue (section 4, row 4).
- No path promotes them (row 5, with row 7 as the defense in depth).
- No path starts them: the Run Now and Start actions go through `queue_move_refusal` and the executor admission.

Their Task records are preserved byte for byte (test T13).

## 4. Enforcement: one predicate at every admission boundary (PROPOSED)

### 4.1 The predicate

A single pure function in the domain layer, for example `domain::project::project_admissible(project: &Project) -> Result<(), ProjectNotAdmissible>`, would return an error when `hidden_at` is `Some`. Every boundary below calls it or filters on it. The refusal text would be defined once.

### 4.2 Why this is the smallest reliable boundary (PROPOSED)

- Most boundaries already read the Project under a lock. `project_after_lock` (`conversation.rs:197-207`) runs under the Conversation lock. `create_checked` checks existence under the gate and the Task write lock (`lifecycle.rs:613`, `625-629`). The predicate would replace or extend those reads. No new lock order is introduced.
- Frontend filtering is not a boundary. Tauri commands and the IPC channel reach the same lifecycle, conversation, queue, and worktree functions. A filter in the webview would not stop a call from another front door.
- Background loops are not front doors and must be covered too. The executor's polling loop and PR refresh act without any user request, which is why rows 5, 7, and 12 exist.

### 4.3 Boundary table

Each row is a location where an active operation would call the predicate, or a read surface that would filter on it. Verified locations are given. Callers that were not classified are listed in section 13.

| # | Boundary | Verified location | Proposed behavior while hidden | Label |
|---|---|---|---|---|
| 1 | Conversation send, act, retry, continuation | `conversation.rs:197-207`; callers at 354, 401, 652, 811 | Refuse every entry point, including open and get (219, 258). Both can write: `open_project_conversation` calls `load_or_create` (which creates the file), and `get_project_conversation` persists recovered Run state. Viewing a hidden Project's Conversation would need a separate read-only path, which this proposal does not cover. | PROPOSED |
| 2 | Single Task creation (including IPC create) | `lifecycle.rs:586-640`; check at 625-629; `commands/task.rs:133`; `ipc/handlers.rs`; `src-tauri/src/conversation_actions.rs:437` and `:1166` | Replace the existence check with the predicate. | PROPOSED |
| 3 | Task import and batch | `lifecycle.rs:648-668`; `commands/jira.rs:232`; `commands/github.rs:281`, `:406` | Same as row 2. | PROPOSED |
| 4 | Move or enqueue into Queue | `lifecycle.rs:869`, `913`, `982`, `997`; `commands/task.rs:231`, `287`, `940`; `ipc/handlers.rs:305`, `:376`; `commands/human_review.rs:611`, `:625` (request-changes re-queue) | Refuse a move into Queue for a hidden Project. Found while tracing enqueue paths. | PROPOSED |
| 5 | Queue promotion | `queue/manager.rs:55-90`; `queue/executor.rs:2360-2390` | Remove tasks of non-admissible Projects from the candidate map before `select_promotable`. Keep `select_promotable` pure. | PROPOSED |
| 6 | Check-and-execute loop | `executor.rs:2574` | No separate check; relies on rows 5 and 7. | PROPOSED |
| 7 | Execution defense in depth | `executor.rs:5345` (`start_execution_under_lease`, after the lease, before the run); `acquire_task_worktree` (`:3847`); `execute_task` (`:6201`); `start_new_attempt` (`:5141`); run start in `record_run_start_for` (`:4742`, reads the Project with `try_read`); working directory in `resolve_working_dir_for_task` (`:8283`) | Check the predicate after the lease is held. Covers direct calls that bypass the queue. Each listed function reads the Project and must be covered. | PROPOSED |
| 8 | Worktree creation | `commands/worktree.rs:16-26`; Project lookup at 51 | Check after the lease at line 22, before the start guard and creation. | PROPOSED |
| 9 | Orphan reclaim | `commands/orphans.rs:12-22`; `reclaim_orphan_checkout` at `worktree/orphans.rs:275` | Refuse reclaim for hidden Projects. Scanning may stay read-only. The owner set already protects a hidden Project's checkouts and branches (section 2.4). | PROPOSED |
| 10 | Startup recovery | `app_core.rs:403-418`, comment at 432-435 | No new check in startup itself. Row 7 refuses any handoff to the executor. The same reconciliation also runs in the executor loop; that path is row 24. Test T22 asserts that a hidden Project's recovery writes nothing. | PROPOSED |
| 11 | Project update, including rename | `commands/project.rs:698-750` | Refuse all changes while hidden. An agent-type change would clear `api_key` (section 2.3), and a `repository_id` change would re-derive routes. A rename is harmless but is refused for simplicity. | PROPOSED |
| 12 | Background PR polling and PR refresh | `executor.rs:3258` (`apply_polled_pr_state`); `commands/pr.rs` refresh about 6546-6640 | Skip Tasks of hidden Projects. Background polls write nothing for them. A manual refresh of a hidden Task is refused. | PROPOSED |
| 13 | Human Review decisions, delivery, and PR creation | `commands/human_review.rs`: `approve_task` 105, `create_approved_task_pr` 119, `complete_task_locally` 136, `request_task_changes` 153, `approve_task_again` 257, `retry_delivery` 401, `complete_locally` 441, `request_changes` 583, `deliver` 671 | Refuse for hidden Projects. Hide already refuses `HumanReview` Tasks (section 3.4), so this row mainly guards against a race. | PROPOSED |
| 14 | Project file commands | `commands/file.rs:36` (`project_root_for`) | Refuse for hidden Projects. Reading a hidden Project's files is not needed by any hidden-state view. **Recommendation** (technical default): refuse. | PROPOSED |
| 15 | PR read helpers | `commands/pr.rs:130` (`resolve_repository_dir`) | Read-only helpers may stay allowed. Any write they lead to is refused by row 13. | PROPOSED |
| 16 | Workspace attach and detach | `commands/project.rs:752-812` (attach); `:814-850` (detach) | **Attach refused** while hidden. **Detach allowed**, so a hidden member never blocks a Workspace indefinitely (section 10). | PROPOSED |
| 17 | Active listings and lookups | `commands/project.rs:155-170` (`list_projects`, `get_project`); IPC `handle_list_projects` (`ipc/handlers.rs:112-125`) | Exclude hidden Projects. `get_project` would return `None` for a hidden id, so active callers fail closed. | PROPOSED |
| 18 | Attention summary | `commands/attention.rs:60-67` | Exclude hidden Projects from the summary, even when they hold Failed Tasks. Section 3.4 does not block `Error` Tasks, and `Status::Error` maps to `Stage::Failed` and then `AttentionReason::Failed` (`crates/slashit-attention/src/lib.rs:108`, `:198`). Those signals are deliberately lost while hidden. | PROPOSED |
| 19 | Orphan owner attribution and storage accounting | `worktree/orphans.rs:88-138`; `storage_accounting/inventory.rs:345-374` | **No change.** Checkouts are always protected. Task ids and branches are protected when the Project's repository identity matches the scanned repository or cannot be determined (the unknown case counts as shared). Hidden Projects' Tasks stay owners. | PROPOSED (intended result) |
| 20 | Worker new-attempt authorization and start | `commands/worker_new_attempt.rs:260-300` (`prepare`, Project lookup at 300 checks only `repository_id`); `authorize_new_attempt` (508); `start_new_attempt` (519) | Refuse for hidden Projects inside `prepare`, so both the authorization and the start are refused. Row 7 covers only the executor-side `start_new_attempt`. | PROPOSED |
| 21 | Worker recovery evaluation and acceptance | `commands/worker_recovery.rs:425` (Project read); `accept_worker_result` (476); `accept_result` (597) | Refuse acceptance for hidden Projects. Acceptance can move a Task toward AI Review. Row 10 covers only startup recovery. | PROPOSED |
| 22 | Integration and delivery commands | `commands/integration.rs:180-227` (integrate, complete-and-integrate, resume, discard); Project reads at 249 and 392 | Refuse all writes for hidden Projects. Pending integrations already block hide (section 3.4), so this mainly closes races. | PROPOSED |
| 23 | IPC and GUI status moves to Done or Error | `ipc/handlers.rs:245` (`handle_move_task`) calls `lifecycle::terminalize_leased`; `queue_move_refusal` covers Queue moves (row 4) | Refuse terminalizing moves for hidden Projects. Terminalizing removes a worktree and changes status. | PROPOSED |
| 24 | Committed Worker handoff reconciliation, run by the executor loop | `executor.rs:2589` (called from `check_and_execute`, `:2574`); `reconcile_committed_worker_handoffs` (`:2806`); Project reads at `:2872` and `:2938` | Skip Tasks of hidden Projects before any write. At `:2872` a missing Project records "recovery required", which is a write, so the skip must come before that branch. A hidden Project should have no unsettled handoff (section 3.4, item 6), so this is a defensive skip. | PROPOSED |
| 25 | Dependency checkout admission and Worker-commit verification | `verify_or_requalify_checkout_admission` (`executor.rs:3748`, Project read); `worker_commit_still_verifies` (`:6932`, read-only); `project_base_of` (`:4579`, read-only) | Refuse admission in `verify_or_requalify_checkout_admission` for hidden Projects. The read-only verifiers may stay allowed. | PROPOSED |
| 26 | PR publication and restack | `create_pr_for_approved_task` (`commands/pr.rs:493`), `recover_private_email_and_create_pr` (`:863`), and `create_pr_inner` (`:4028`), which call `create_pr_reserved` (`:4172`) and then `restack_onto_landed_parent` (`:5273`) | Refuse for hidden Projects. These publication entry points sit behind row 13's human-review actions and must be guarded directly, not only through their callers. | PROPOSED |
| 27 | PR republish: restack and discard | Commands `restack_published_task` (`commands/pr.rs:438`, calling `republish::restack` at `commands/pr/republish.rs:477`) and `discard_published_restack` (`pr.rs:449`, calling `republish::discard` at `republish.rs:970`) | Refuse both writes for hidden Projects. `republish::status` (`republish.rs:420`) is read-only and may stay allowed. | PROPOSED |
| 28 | Project path and facts reads | `get_project_path` (`src-tauri/src/commands/project.rs:852`; registered at `src-tauri/src/lib.rs:453`); `project_facts` in `coordinator_turn` (`conversation.rs:709-721`); `repository_paths_by_project` (`app_core.rs:752`) | Refuse `get_project_path` for hidden Projects, as with row 17, so an active lookup cannot return a hidden folder. `project_facts` is reached from `coordinator_turn`; the call chain back to the row 1 gate was not fully traced, so the implementation must confirm it. `repository_paths_by_project` callers were not classified. | PROPOSED |

Rows 12 and 13 are located. Rows 20 to 28 come from a search of Project-map reads (`projects.read()`, `try_read()`, and `write()`) in `src-tauri/src/queue/`, the executor, and the non-test commands, `app_core.rs`, and `lifecycle.rs`. The search is not a proof of exhaustive coverage. The implementation must repeat it for every function before claiming coverage.

## 5. Lifecycle safety (PROPOSED)

### 5.1 Lock sequence

Hide would take locks in the order below. The first three match the order `delete_project` uses for the same locks. Hide takes the in-memory `projects` write lock last, after it has read Tasks under the leases. `delete_project` instead takes the Task board write lock (`project.rs:329`) before the `projects` write lock (`project.rs:389`). That order is acceptable for deletion, but hide does not copy it.

1. The Project Conversation lock, with the live-run refusal (`project.rs:240-256`).
2. The Project lifecycle gate in write mode (`project.rs:216`). This excludes `create_checked`, which takes the gate in read mode (`lifecycle.rs:613`), and `update_project`, which also takes it in read mode (`project.rs:710`).
3. Every Task lifecycle lease of the Project (`lifecycle::hold_project_task_leases`, `lifecycle.rs:1410`).
4. Evaluate the preconditions in section 3.4, reading Tasks under those leases.
5. The in-memory `projects` write lock, held across clone, persist, and adopt, the same as `create_project_committed`, `update_project_committed`, and the attach path (section 2.1).
6. Set `hidden_at` on the cloned Project and persist the table through `try_persist_projects` (one `update_config`, `storage.rs:929-940`).
7. Adopt the published value (`adopt_published`), then release the locks in reverse order.

Restore would follow the same sequence, with the checks of section 3.5 in step 4.

Lock-order rule for the implementation: no code may hold the `projects` write lock while waiting for the gate or the Conversation lock. `create_checked` reads `projects` while holding the Task write lock (`lifecycle.rs:625-629`); that is consistent with this order because hide holds the gate before it takes `projects`.

### 5.2 Why no pending-record protocol is needed (PROPOSED)

Deletion needs a multi-step protocol because it removes files in several places: the Task board, the Conversation file, and the config entry (`project.rs:283-296`; startup cleanup at `app_core.rs:236-262`). Hide removes nothing. It changes one field in one file, written by an atomic replace. A crash leaves either the old file or the new file. The pending-record machinery would protect against partial multi-file state, and that state cannot arise here.

### 5.3 Races (PROPOSED)

- **Task creation.** A creation waiting on the gate (`lifecycle.rs:613`) finishes before hide takes the gate in write mode. If it created a Backlog Task, that Task has no checkout and hide can proceed; the hidden Project then holds that Backlog Task. Any creation after hide is refused by row 2.
- **Lease waiters.** A start that is waiting on a lease re-checks the predicate after it acquires the lease (row 7). Hide holds every lease, so a waiter sees the `hidden_at` that hide adopted before it released them.
- **Concurrent Project writers (lost update).** `try_persist_projects` writes the whole table from an in-memory clone. If `update_project`, `create_project`, or attach cloned the map before hide adopted `hidden_at` and then persisted, their write would revert the hide on disk. This is prevented because every writer takes the same `projects` write lock across clone, persist, and adopt (step 4 above). The implementation test T26 must cover it.
- **Concurrent hide and restore.** Both take the same gate and lease sequence, so two operations on one Project serialize.

### 5.4 Crash and failure (PROPOSED)

| Moment | Config on disk | Memory after restart | Operations that run |
|---|---|---|---|
| Before the lock sequence | Visible | Visible | Normal |
| After the locks, before the rename | Visible | Visible; the attempt is abandoned when the locks are released | Normal |
| During the rename | Old or new content (atomic replace) | Matches disk after reload | Matches disk |
| After the rename, before the directory sync | New (usually) | Hidden in this process; may revert after power loss | Refused in this process; visible after a revert |
| After a confirmed write | Hidden | Hidden | Refused at every boundary in section 4 |

The one window that can revert is the unconfirmed directory sync. It is reported to the user and reconfirmed at the next start (section 5.5).

### 5.5 Failure details (PROPOSED)

- **Failure before the rename.** `update_config` returns `NotPublished` (`storage.rs:1024-1026`). Memory and disk keep the Project visible, and the user sees a failed hide.
- **Crash before the rename.** The Project stays visible. This is the status quo.
- **Crash after the rename.** The Project is hidden. Every boundary reads the persisted flag, so no operation starts after restart.
- **`PublishedNotDurable`** (`storage.rs:1015-1022`). Memory adopts `hidden_at`, and the error reports that durability is unconfirmed. Power loss before the directory sync could revert the file to visible. The proposal would report "hide not confirmed" to the user and rely on `reconfirm_config_durable` (`storage.rs:977-983`). Startup calls it today only when other deletions are owed (`app_core.rs:236-257`). The proposal would extend the startup call to run whenever the configuration is unconfirmed.
- **Restore after an interrupted write** follows the same table with the roles reversed. A crash before the rename leaves the Project hidden, and the user retries.

## 6. Startup, restart, surfaces, and risks

### 6.1 Startup (PROPOSED)

- Routes for hidden Projects remain, because `refresh_routes` iterates every Project (`storage.rs:1033-1050`). Their boards load as they do today.
- The reconciliation loop (`app_core.rs:403-418`) visits every Task. Under section 3.4, a hidden Project's Tasks have no checkout and no unfinished marker, so the loop writes nothing for them.
- Migration (`app_core.rs:766`) can rewrite an old-format board on the first start. That is existing behavior. The byte-identity test (T13) uses current-format fixtures so that migration does not affect it.
- Test T22 asserts that `build_state` starts no run for a hidden Project and leaves its Task and Conversation files unchanged.

### 6.2 Surfaces (PROPOSED)

- **Hide and restore are GUI-only** and take `GuiMutation`, as `delete_project` does (`commands/project.rs:90`, `:174`). Neither the IPC nor the CLI would list or modify hidden Projects. This is a recommendation: an unattended IPC or CLI call could otherwise hide a Project.
- **Hidden listing** is a separate read-only command (section 7). It returns a summary, not the Project record.
- The GUI would show a "Removed Projects" area in Settings with name, repository path status, and hidden date. The area would offer Restore only where section 3.5 allows it, and the copy would say why otherwise.

### 6.3 Risks (PROPOSED unless marked)

- **Readers that treat presence in `projects` as "active".** Hidden Projects stay in the map, so every reader that iterates it still sees them. The predicate covers admission, and rows 17-18 cover display. Verified intended cases: orphan owners and storage accounting keep attributing a hidden Project's checkouts and branches to it (row 19).
- **Long lease holds.** Hide takes every Task lease. A long-running holder delays hide. Hide refuses when live work exists, so the wait is bounded by work the refusal would report.
- **Downgrade.** Section 8. Not fixable in the new build.
- **Unlocated boundaries.** Any caller of an existing function that the search did not find would be a gap in admission, not in the predicate. The implementation must repeat the search for each function in the table.

## 7. Credentials and exposure (VERIFIED CURRENT facts; PROPOSED policy)

**Verified.**

- `list_projects` and `get_project` return complete Project records, including `agent_config.api_key` (`commands/project.rs:155-170`). The Tauri commands therefore send configured keys to the webview today, for every Project. This is a separate security finding and is recorded here, not fixed by hide.
- `update_project` has no `api_key` parameter (`project.rs:698-705`). But an agent-type change replaces `agent_config` with a default that has `api_key: None` (`project.rs:737`, `:69-88`). **Ordinary edits can therefore clear a configured key.** 
- The IPC list does not expose `api_key` (`ipc/handlers.rs:112-125`).
- `api_key` is stored in `config.toml`, written with `OwnerOnly` permissions (`storage.rs:1356-1361`; `paths.rs:21` describes the 0600 contract). The code does not encrypt it. This design does not claim encryption.

**PROPOSED.**

- **Hidden Projects never reach the webview through the new surfaces.** Hide and restore add no secret field. The hidden listing (`list_hidden_projects`) returns a summary type with `id`, `name`, `hidden_at`, and repository path status only. The type has no `AgentConfig` field, so there is nothing to redact. Restore takes the id.
- **Active listings and lookups exclude hidden Projects** (section 4, row 17), so a hidden key is also not returned by `list_projects` or `get_project`.
- **Hide and restore never change `api_key`.** The single-field invariant (section 3.2, test T14) covers it.
- **Refuse all updates while hidden** (row 11), so the agent-type reset cannot run for a hidden Project.
- **Active-Project exposure is out of phase 1.** Returning `api_key` for visible Projects through `list_projects` and `get_project` is a separate finding. Redacting it would change the active settings UI, which is unrelated to hide. **Decision O2** records whether to schedule that work.

## 8. Downgrade and compatibility (VERIFIED CURRENT facts; PROPOSED policy)

### 8.1 Facts

- Released `v0.0.2` and `v0.0.3` ignore unknown keys in a Project table (section 2.7). An older build loads a hidden Project as active.
- Any later write from an older build re-serializes the whole `AppConfig` from that build's structs (`storage.rs:929-940`, `:1001-1005`). `try_persist_projects` rebuilds the `projects` table from its own `Project` values (`project.rs:33-48`). The `hidden_at` key is dropped by that write, and the Project stays active.
- A new build cannot prevent this. Code in the new build does not run inside an older binary.

### 8.2 Consequence for hidden Tasks

Section 3.4 refuses hide when any Task is in an active or awaiting-decision state, has a checkout, or has an unfinished marker. So a hidden Project's Tasks are inert in an older build too. An older build would show the Project, and a person could move a Backlog Task into Queue by hand, which would start work. That is the real exposure, and the downgrade procedure must prevent it by restoring first.

### 8.3 Alternatives considered

- **A separate hidden-state file.** Released builds never read it, so nothing stops them from reactivating a Project. It also breaks the single-file invariant.
- **A key in `features.toml`.** Not verified, so not evaluated.
- **A non-UUID id, or an unknown enum value such as an unknown `state_location`.** An older build would then fail to deserialize the configuration. The blast radius (every Project) is larger than the risk it addresses. Not verified.
- **Intentional corruption to block older binaries.** Rejected. It is outside phase 1.

### 8.4 Proposed policy (PROPOSED)

1. **Documented downgrade procedure.** Before running an older build against the same configuration, restore every hidden Project. The procedure states that an older build reactivates hidden Projects silently and that its Tasks can then be queued by hand.
2. **Configuration backup.** No backup mechanism exists in the code (verified only as far as the config write path, not exhaustively). The procedure recommends copying the config directory before any downgrade or manual edit. Hide itself adds no backup.
3. **Startup notice (later, not required for phase 1).** A later build may list the hidden Projects it loaded, in a non-blocking and non-intrusive way. The notice would be informational only. It is not a guarantee against older builds, and phase 1 does not depend on it.
4. **Compatibility safeguard for phase 1: none.** A detection marker in a separate file, written at hide time, would let the new build warn when `hidden_at` disappeared after a downgrade. It would not block anything, and it would break the single-file invariant. Recommended for a later phase if evidence shows the procedure is not followed.
5. **No fail-closed claim.** The proposal does not claim that older builds enforce hiding. **Decision O1** accepts or rejects this policy.

## 9. Deferred, not part of this phase

- **Permanent deletion** of hidden Projects' records and history. `delete_project` (`project.rs:173-238`) stays unchanged. Hide must never call it. No erase path exists in this phase (D7).
- **Moved-folder recovery.** A moved folder is not restored. Restore reports the moved path and refuses (section 3.5).
- **Repository copy or move.** The Repository record stays where it is. Shared Repository ownership is not changed.
- **Root-commit fingerprinting** for identity. Not adopted.
- **Project key storage.** The documentation mismatch in section 2.2 is separate work.
- **Stored resolved `state_location`.** Storing the resolved location at hide time would fix the `Auto` drift risk (section 3.5 item 4) but needs a second field, which is outside the single-field invariant. Deferred to a later phase.
- **Background PR refresh for hidden Projects.** Deferred (row 12 skips them).

## 10. Workspace membership (VERIFIED CURRENT; consequence PROPOSED)

**Verified.** `projects_referencing` counts every member (`commands/workspace.rs:77-83`), and `delete_workspace` refuses while any member exists (`workspace.rs:104-111`).

**Consequence under this proposal.**

- Membership is kept, so a hidden member still blocks Workspace deletion. The person must restore the Project, or detach it.
- Detach is allowed while hidden (row 16). This matters because a hidden Project whose folder is unavailable cannot be restored; without detach, its Workspace could never be deleted.
- Attach is refused while hidden (row 16), so a hidden Project does not gain a new Workspace.
- **Decision O4** accepts that a hidden member blocks Workspace deletion until it is restored or detached.

## 11. Acceptance criteria and focused tests

All tests are PROPOSED. None exists today. "Fixture" names an existing test whose setup can be reused.

| # | Test | Asserts | Fixture |
|---|---|---|---|
| T1 | Hide refused with a Task in Queue, InProgress, AiReview, or HumanReview | Refusal naming the Task; config and files unchanged | New |
| T2 | Hide refused with a running agent | Refusal | `project.rs:2174` |
| T3 | Hide refused with a delivery in flight | Refusal | `project.rs:2193` |
| T4 | Hide refused when any Task has `worktree_path`, in any status | Refusal; checkout untouched | New |
| T5 | Hide refused with a live Project Conversation Run | Refusal | `project.rs:1595` |
| T6 | Hide refused with a pending integration, restack, or cleanup, or an unsettled worker handoff or new attempt | Refusal, one case each | `project.rs:2107`, `:2131`, `:2155` |
| T7 | Hidden Project rejects conversation send, act, retry, open, and get (row 1) | Error; Conversation bytes unchanged | New |
| T8 | Hidden Project rejects single creation, IPC creation, and batch import (rows 2, 3) | Error; Task file bytes unchanged | `project.rs:2950`, `:3002` pattern |
| T9 | Hidden Project rejects a move into Queue (row 4) | Error; status unchanged | New |
| T10 | Promotion never selects a hidden Project's Task, even with capacity (row 5) | `select` returns None for it | `queue/manager.rs:293-400` pattern |
| T11 | `start_execution_under_lease` and `acquire_task_worktree` refuse a hidden Project when called directly (row 7) | Refusal; no run started | New |
| T12 | `create_worktree` refuses a hidden Project (row 8) | Refusal; no directory created | New |
| T13 | Hide and restore preserve every Task file and Conversation file byte for byte | Byte equality | Fixtures from T7 and T8 |
| T14 | Hide and restore change only `projects.<id>.hidden_at` in `config.toml` (section 3.2) | Tree diff has one key; `api_key` unchanged | New |
| T15 | Hide does not touch the repository directory, branches, refs, worktrees, or any file other than `config.toml` | Snapshot equality | Git helper at `commands/worktree.rs:183` |
| T16 | Restore with a moved or missing folder is refused, with no new Project id and no Add suggestion | Refusal; no write | New |
| T17 | `list_projects` and `get_project` exclude hidden Projects, and `list_hidden_projects` contains no `api_key` field | Serialized output has no key material | New |
| T18 | `update_project` refuses any change while hidden, including agent-type changes (row 11) | Refusal; config and `api_key` unchanged | New |
| T19 | `delete_workspace` refused with a hidden member; `detach_project_from_workspace` allowed; attach refused (row 16) | Three outcomes | `workspace.rs:154-181` pattern |
| T20 | A failing config write leaves memory and disk unchanged | Equality | `project.rs:1204`, `:1223` pattern |
| T21 | `PublishedNotDurable` on hide: memory hidden, error reported, reconfirmed on next start | Behavior as in section 5.5 | New |
| T22 | `build_state` with a hidden Project starts no run and writes no Task or Conversation file | No change | New |
| T23 | Hide never calls `delete_project`'s path (no pending deletion record is created) | Record directory empty | New |
| T24 | A config without `hidden_at` loads and round-trips unchanged | Equality | New |
| T25 | An older-build downgrade is documented, not automated | Manual check only | None |
| T26 | A concurrent `update_project`, `create_project`, or attach that started before hide cannot revert `hidden_at` on disk (section 5.3) | Disk shows hidden after both complete | New |
| T27 | A repository-less Project hides, restores without a path check, and keeps its UUID | Round trip; no Repository created | New |
| T28 | Adding a folder whose canonical path a hidden Project's Repository uses is refused before any record is written, and the message names the hidden Project (section 3.6) | Refusal; no new Repository or Project | New |
| T29 | A hidden Project's Backlog Task without a checkout survives hide and restore unchanged, and is not promoted while hidden (sections 3.7, row 5) | Bytes equal; status Backlog | New |
| T30 | Background PR polling writes nothing for a hidden Project's Tasks (row 12) | No write observed | New |
| T31 | File commands refuse a hidden Project (row 14) | Refusal | New |
| T32 | Orphan scan lists no checkout or branch of a hidden Project as reclaimable (row 19) | Owner set contains them | New |

**Desktop acceptance (PROPOSED).** One journey: add a Project with a folder, create a Task, hide the Project, confirm it is absent from the list, restart, add the same folder (refused with the restore message), restore, and confirm the Task is present. The existing harness in `crates/slashit-acceptance` would host it. The journey does not exist today. It would be a later implementation item.

## 12. Decision record

### 12.1 Decisions already taken (direction, not open)

- **D1 Logical hide.** Keep the record; add one field (section 3.1).
- **D2 Hidden means inactive.** Enforced in the backend at every boundary (section 4).
- **D3 Refuse unsafe hide.** Refuse on live work, a checkout, an active queue or admission, an unresolved delivery or integration, and other ambiguous durable work. Inert Backlog Tasks may remain (section 3.4). The guard is defined from the markers in section 2.5.
- **D4 Restore existing identity.** Same UUID and history. Linked repositories must match the original record and location. Moved folders and relinking are deferred. Repository-less Projects are restorable without invention (section 3.5).
- **D5 Preserve Workspace membership.** Kept; deletion refusal documented (section 10).
- **D6 Preserve credentials, restrict exposure.** Kept on disk and not returned for hidden Projects (section 7). The active-Project exposure is recorded.
- **D7 No permanent deletion.** Not in phase 1 (section 9).

### 12.2 Phase-1 decisions

Former items O3 and O5 are settled by D3 and D7 and are not listed here.

The owner has settled O1, O6 and O7 for phase 1 at the defaults shown below. O2 and O4 remain open; each default applies until the owner decides otherwise.

- **O1. Downgrade policy. Settled: accept.** A documented procedure, with the limitation in section 8. Section 8.4.
- **O2. Active-Project `api_key` exposure. Open.** Leave unchanged in phase 1 and schedule as a separate security item (default), or include active-Project redaction in this phase. Section 7.
- **O4. Workspace-delete consequence. Open.** Accept that a hidden member blocks Workspace deletion until it is restored or detached (default). Section 10.
- **O6. Open pull requests on hide. Settled: warn and do not block.** Section 3.4.
- **O7. Add with a hidden Project's folder. Settled: refuse and point to restore.** Section 3.6.

### 12.3 Recommendations that are technical defaults (not decisions)

- Hide and restore are GUI-only mutations (section 6.2).
- File commands refuse for hidden Projects (row 14).
- Update refuses all changes while hidden (row 11).
- `get_project` returns `None` for a hidden id to active callers (row 17).
- Attach refused, detach allowed while hidden (row 16).

### 12.4 Settled by the source, not listed as choices

The lock order and the write path are resolved from the code (sections 2 and 5). The boundary list and the restore identity check remain open (section 13).

## 13. Unknowns and checks left for implementation

- The exact repository identity call for restore (section 3.5 item 3). The orphan scan uses `repository_identity`, but its behavior for a missing path was not read in full.
- Whether a Task in `HumanReview` keeps its `worktree_path` in every case. The design refuses `HumanReview` by status regardless, so the answer does not change the outcome.
- Whether `create_project` takes the gate. This matters only for the lock-order description; the `projects` write lock is the serializing point either way.
- The IPC create handler's exact call chain to `lifecycle::create`, and whether it checks Project admission before the call. The design makes `create_checked` the single enforcement point.
- The Conversation Coordinator path that approves a `CreateTask` action: whether it reaches `create_checked` or another creation function. Row 1 covers the conversation entry points, and the implementation must search for every caller of `lifecycle::create*`.
- Whether any reader outside the listed boundaries reads `projects` to decide activity. Rows 24 to 28 came from the search described in section 4.3. The implementation must repeat that search and confirm the unclassified callers: `repository_paths_by_project` (`app_core.rs:752`) and the call chain for `project_facts` (`conversation.rs:709`).
- Whether any path holds Task state and then waits for the `projects` lock, or the reverse (for example `executor.rs:2872` and the `try_read` at `executor.rs:4742`). The lock-order rule in section 5.1 assumes none does.
- Resolved: `AgentConfig` serializes `api_key`. It has no skip attribute (`src-tauri/src/domain/project.rs:138-144`), so the configured key is returned by the active-Project commands. Section 7 records the consequence.
- The body of `projects_referencing` was not read; only its call site in `delete_workspace` (`workspace.rs:104-105`) was.
- The downgrade behavior of older builds was verified by the absence of `deny_unknown_fields` and `flatten`. It was not run against a real older binary.
- Desktop behavior of the Removed Projects view and the copy has not been checked. No GUI check has been made.

## 14. Future phases

- Permanent deletion of hidden Projects, using the canonical board route and the pending-cleanup protocol. Whether to add it at all is a later decision (D7 excludes it from phase 1).
- Moved-folder recovery and relinking.
- A stored resolved `state_location`, to remove the `Auto` drift risk.
- A downgrade detection marker in a separate file, if evidence shows the documented procedure is not followed.
- Active-Project `api_key` redaction, if O2 selects it.
- Background PR refresh for hidden Projects, if the warning in O6 proves insufficient.
