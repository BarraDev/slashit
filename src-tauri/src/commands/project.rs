use crate::domain::{Project, ProjectScope, AgentType, AgentConfig, Task};
use crate::config::{Storage, WorkspaceRegistry};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type Projects = Arc<RwLock<HashMap<Uuid, Project>>>;

/// Persist projects, reporting failure to the caller.
///
/// Most callers are content to log and continue, but a caller that has already
/// moved files on disk cannot: losing the write there would leave the project
/// pointing at the directory the data just left, and the user would open an
/// empty board with no indication that anything went wrong.
///
/// Loading the current config is not allowed to fail silently into
/// `AppConfig::default()`: a transient read failure (permissions, a momentary
/// I/O error) must not be papered over by writing back a blank config, which
/// would destroy `jj_config`, `ui_preferences`, `agent_configs`, and whichever
/// of projects/repositories this call was not updating. The error is
/// propagated instead so the caller can surface it rather than silently
/// "succeeding" at wiping the user's config.
///
/// The read and the write go through `Storage::update_config` so they are one
/// transaction. Loading the config here and saving it back as two separate
/// steps would let a concurrent repository save — which holds a different
/// in-memory lock and so is not excluded by the caller's `projects` guard —
/// land in between, and whichever write went second would revert the other's
/// section.
pub(crate) fn try_persist_projects(
    storage: &Storage,
    projects: &HashMap<Uuid, Project>,
) -> anyhow::Result<()> {
    // No extra `context` here on purpose: every caller formats this error
    // with `{e}`, which renders only the outermost context, so wrapping it
    // would replace the stage that actually failed ("Failed to load config
    // before updating it" / "Failed to write config file") with a tautology.
    // The callers already name the operation.
    storage.update_config(|config| {
        config.projects = projects
            .iter()
            .map(|(id, project)| (id.to_string(), project.clone()))
            .collect();
    })
}

#[derive(Clone)]
pub struct ProjectState {
    pub projects: Projects,
}

impl ProjectState {
    pub fn new() -> Self {
        Self {
            projects: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for ProjectState {
    fn default() -> Self {
        Self::new()
    }
}

fn get_default_agent_config(agent_type: &AgentType) -> AgentConfig {
    let (command, args) = match agent_type {
        AgentType::ClaudeCode => ("claude", vec!["--stdio"]),
        AgentType::Cursor => ("cursor-agent", vec!["--stdio"]),
        AgentType::Cody => ("cody-agent", vec!["--stdio"]),
        AgentType::Continue => ("continue-agent", vec!["--stdio"]),
        AgentType::Other(_) => ("unknown", vec![]),
    };

    AgentConfig {
        agent_type: agent_type.clone(),
        command: command.to_string(),
        args: args.into_iter().map(String::from).collect(),
        env: HashMap::new(),
        model: None,
        api_key: None,
    }
}

#[tauri::command]
pub async fn create_project(
    state: tauri::State<'_, crate::AppState>,
    name: String,
    repository_id: Option<String>,
    agent_type: AgentType,
) -> Result<Project, String> {
    let id = Uuid::new_v4();
    let repository_id = repository_id
        .and_then(|r| Uuid::parse_str(&r).ok());

    let agent_config = get_default_agent_config(&agent_type);
    let now = chrono::Utc::now();

    // A repository that says unambiguously which branch it is on gives the
    // project its local base now, so that tasks keep starting there however
    // the primary checkout moves later. See `worktree::project_base`.
    let repository_path = match repository_id {
        Some(id) => state.repository.repositories.read().await.get(&id).map(|r| r.local_path.clone()),
        None => None,
    };
    let base = crate::commands::repository_setup::captured_base(repository_path.as_deref()).await;

    let project = Project {
        id,
        name,
        repository_id,
        scope: crate::domain::ProjectScope::Standalone,
        // New projects keep the repository pristine. Storing the board inside
        // the project is a deliberate opt-in, never the default.
        state_location: crate::config::paths::StateLocation::External,
        base,
        agent_type,
        agent_config,
        created_at: now,
        updated_at: now,
    };

    create_project_committed(&state.project.projects, &state.storage, project).await
}

/// Core of [`create_project`], taking the raw map and storage so tests can
/// call it without a `tauri::State`.
///
/// Inserts into a cloned map, persists the clone, and only replaces the
/// shared map on success — a failed persist never leaves memory holding a
/// project the caller was told did not get created.
async fn create_project_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    project: Project,
) -> Result<Project, String> {
    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();
    proposed.insert(project.id, project.clone());

    try_persist_projects(storage, &proposed)
        .map_err(|e| format!("Failed to persist project: {e}"))?;

    *projects_w = proposed;
    Ok(project)
}

#[tauri::command]
pub async fn list_projects(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<Project>, String> {
    let projects = state.project.projects.read().await;
    Ok(projects.values().cloned().collect())
}

#[tauri::command]
pub async fn get_project(
    state: tauri::State<'_, crate::AppState>,
    id: String,
) -> Result<Option<Project>, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    let projects = state.project.projects.read().await;
    Ok(projects.get(&id).cloned())
}

#[tauri::command]
pub async fn delete_project(
    state: tauri::State<'_, crate::AppState>,
    id: String,
) -> Result<bool, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;

    // Serialize the deletion against every Project Conversation mutation
    // FIRST, before the gate: a Coordinator Project action (an approved
    // CreateTask, say) runs `lifecycle::create` while holding this same
    // Conversation lock, and that create waits on the gate. If the deletion
    // held the gate and then waited here, the two would meet head-on -- each
    // waiting for the lock the other holds -- and park forever. With this
    // order both sides agree on Conversation-before-gate, and the deletion
    // still excludes them for its whole transaction: nothing conversation- or
    // gate-side can start while it commits. An active provider must finish or
    // be stopped before its owned durable history is removed, otherwise its
    // completion could recreate an orphan.
    let running = state
        .executor
        .get()
        .map(|executor| executor.as_ref() as &dyn crate::lifecycle::ExecutionOwnership);
    let _conversation_guard = lock_project_conversation_for_deletion(
        &state.conversation,
        &state.storage,
        id,
        |conversation_id| {
            state
                .executor
                .get()
                .is_some_and(|executor| executor.project_run_is_live(conversation_id))
        },
    )
    .await?;

    // With the Conversation lock held, the deletion takes the Project's
    // lifecycle gate exclusively, before any Task is read or any lease taken:
    // no Task creation can be born into the board, the durable Task records,
    // or a lifecycle transition while this transaction decides. Lock order
    // across the whole backend stays acyclic -- Conversation, gate, task
    // leases, shared board -- and every Task-related holder of the gate (a
    // create, an approved Project action) reaches it after this deletion has
    // already been excluded or has already finished.
    let _gate = state.task.project_gate.write(id).await;

    // Every Task of this Project takes its lifecycle lease, so no run can
    // start and no checkout can be created for it while the deletion decides
    // and commits. Nothing that holds one of these leases ever waits on the
    // Conversation lock or the gate after this point: the only transition that
    // could have (an approved Project action) is already frozen above.
    let _task_leases = crate::lifecycle::hold_project_task_leases(
        &state.task_lifecycle_locks,
        &state.task.tasks,
        id,
    )
    .await?;
    delete_project_committed(
        &state.project.projects,
        &state.task.tasks,
        &state.storage,
        running,
        Some(&state.task.deliveries),
        id,
    )
    .await
}

async fn lock_project_conversation_for_deletion(
    conversations: &crate::commands::conversation::ConversationState,
    storage: &Storage,
    project_id: Uuid,
    project_run_is_live: impl Fn(Uuid) -> bool,
) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
    let guard = conversations.project_lock(project_id).lock_owned().await;
    if let Some(conversation) = storage
        .load_primary_conversation(project_id)
        .map_err(|error| error.to_string())?
    {
        if project_run_is_live(conversation.id) {
            return Err("Stop the active Project Conversation Run before deleting this Project".into());
        }
    }
    Ok(guard)
}

/// Core of [`delete_project`]. The caller holds, in acquisition order, the
/// Project Conversation's mutation lock, the Project's lifecycle gate in
/// write, and every Task of the Project's lifecycle lease, and keeps them all
/// the way through this call.
///
/// Nothing is refused after anything is committed: while the board's write
/// guard is held from before the first read to the last, the Task set is
/// frozen — no creation can enter (the gate the caller holds, which `create`
/// and `create_batch` honor before their insert), and no transition of a
/// Task already here can run (its lease is held). All refusals — a live run,
/// a pull request being opened, an unfinished integration, restack, cleanup
/// or a checkout that remains — are answered before anything on disk changes.
///
/// When nothing is owed, the durable sequence is the one
/// [`Storage::delete_project_conversation`]'s protocol uses, extended to
/// Tasks: write a cleanup record for the Task records *before* the Project is
/// removed from config, persist the Project removal, then delete the Task
/// records (refusing loudly, and leaving the record, if that fails), then
/// remove the Tasks from the shared board, and only then the Conversation and
/// its record. A failure after the Project removal committed is reported as
/// exactly that — "deleted, cleanup pending" — never as success, and the next
/// start finishes the cleanup from the record. The record for a Project the
/// config had already lost (a retried deletion) is written before its Task
/// files are deleted, so a failed attempt against orphans still says what the
/// next start must finish.
async fn delete_project_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    tasks: &crate::lifecycle::Tasks,
    storage: &Storage,
    running: Option<&dyn crate::lifecycle::ExecutionOwnership>,
    deliveries: Option<&crate::commands::human_review::DeliveriesInFlight>,
    id: Uuid,
) -> Result<bool, String> {
    // The board-file capture sits inside the gate: a route-changing update
    // (a `repository_id` change re-derives the routes) is not racing the
    // capture out of date — the update cannot even start until this
    // transaction ends, so the routes captured here are the routes the
    // durable read and the config removal below both see.
    let board_files = storage.tasks_board_files(id);
    // A record left by an unfinished earlier attempt makes its cleanup owed
    // even when the durable file has since gone empty or lost: exactly what
    // the next start would otherwise reconcile.
    let pending_record = storage
        .has_pending_tasks_deletion_record(id)
        .map_err(|e| format!("Failed to read pending Task cleanup records: {e}"))?;
    // A cleanup record this Process cannot write faithfully is a deletion
    // this Process must refuse: fail closed before anything is committed.
    if let Err(reason) =
        crate::config::storage::Storage::check_cleanup_paths_losslessly_recordable(&board_files)
    {
        return Err(format!(
            "This Project cannot be deleted: its Task records live at paths the pending cleanup \
             record cannot faithfully carry ({reason}). Nothing was deleted; rename or move the \
             offending state location before deleting the Project."
        ));
    }

    let durable = storage.load_project_tasks(id).map_err(|e| {
        format!(
            "This Project's Task records cannot be read, so no check can prove that deleting \
             it would not strand unfinished work; the records on disk must be put right first: {e}"
        )
    })?;

    // --- Refusal pass. Nothing durable has happened yet. -----------------
    //
    // Held the whole way through, this write guard is what makes the Task set
    // below trustworthy: Task creation is fenced by the gate the caller
    // holds, every other Task mutation holds its Task's lifecycle lease (also
    // held), so nothing can add a Task or append a refusal fact between this
    // look and the commit that follows it.
    let mut tasks_w = tasks.write().await;

    for task in durable
        .iter()
        .chain(tasks_w.values().filter(|t| t.project_id == id))
    {
        if let Some(message) =
            live_work_refusal(task, running, deliveries).await
        {
            return Err(message);
        }
        if let Some(message) = project_task_refusal(task) {
            return Err(message);
        }
    }

    let had_tasks = !durable.is_empty() || pending_record;

    // Test-only rendezvous. Compiled out of release builds; a test arms it to
    // hold the deletion here, between the durable read and the commit, which
    // is exactly the await window the production code already has at the lock
    // acquisitions in between.
    #[cfg(test)]
    delete_junction(id).await;

    // One more look at the facts no belt-and-braces free: a delivery marker
    // is a process-internal flag an `approve` can raise without holding this
    // board's write guard, so its owner parks on the guard while the marker
    // already says "this Task's delivery is starting". Refusing here is still
    // a refusal before anything is committed.
    let commit_window = || async {
        for task in durable
            .iter()
            .chain(tasks_w.values().filter(|t| t.project_id == id))
        {
            if let Some(message) =
                live_work_refusal(task, running, deliveries).await
            {
                return Some(message);
            }
        }
        None
    };
    if let Some(message) = commit_window().await {
        return Err(message);
    }

    // --- Commit the Project removal from config --------------------------
    //
    // Read, mutate, persist and publish inside one write guard, the
    // read-modify-write create_project_committed holds, so another Project
    // save that is only briefly faster cannot be reverted by this one's
    // write. The two per-Project cleanup records go down first, so a crash
    // after this removal names what the next start must finish; a refused
    // persist clears both again, and no destructive step has run.
    // The board files were captured before the durable read and the routes
    // cannot move under the gate: a route-changing update is fenced for the
    // whole transaction, so the capture, the read and the removal agree.

    let removed = {
        let mut projects_w = projects.write().await;
        let mut proposed = projects_w.clone();
        let removed = proposed.remove(&id).is_some();
        if removed {
            storage
                .mark_project_tasks_deletion_pending(id, &board_files)
                .map_err(|e| format!("Failed to record Project Task cleanup: {e}"))?;
            if let Err(error) = storage.mark_project_conversation_deletion_pending(id) {
                storage
                    .clear_project_tasks_deletion_pending(id)
                    .map_err(|clear_error| {
                        format!(
                            "Failed to record Project Conversation cleanup: {error}; also failed \
                         to clear its pending Task cleanup record: {clear_error}"
                        )
                    })?;
                return Err(format!(
                    "Failed to record Project Conversation cleanup: {error}"
                ));
            }
            if let Err(error) = try_persist_projects(storage, &proposed) {
                // Config still holds the Project on disk, so memory must not
                // lose it either. Clear both retry records and say so.
                storage
                    .clear_project_tasks_deletion_pending(id)
                    .map_err(|clear_error| {
                        format!(
                            "Failed to persist project deletion: {error}; also failed to clear \
                         its pending Task cleanup record: {clear_error}"
                        )
                    })?;
                storage
                    .clear_project_conversation_deletion_pending(id)
                    .map_err(|clear_error| {
                        format!(
                            "Failed to persist project deletion: {error}; also failed to clear \
                         its pending Conversation cleanup record: {clear_error}"
                        )
                    })?;
                return Err(format!("Failed to persist project deletion: {error}"));
            }
            *projects_w = proposed;
        }
        removed
    };

    // --- Task-record cleanup, already inside this write guard -------------
    //
    // The guard was taken before the durable read and stays for the tail, so
    // no Task mutation can persist a record of a Project being deleted while
    // the halves of the commit are in between: the file loses the record
    // first, then the board does, and in between nothing can save what the
    // file no longer has. Nothing new is taken inside the tail -- no
    // subprocess, no lock wait, no other lock -- which is what keeps it
    // acyclic; it is safe only beside the guards its caller already holds and
    // keeps holding into it: the Project's lifecycle gate in write, every
    // Task's lifecycle lease and the Project Conversation mutation lock.

    // The replay record carries the captured board files, so this deletion
    // reaches a routed project's file too -- the route itself is gone from
    // the config by now, and recomputing one would strand the file forever.
    // For a Project the config had already lost (a retried deletion, or an
    // orphan Task file without its Project), the record is written HERE,
    // before anything is deleted: a failed attempt must leave the retry
    // reference the next start would follow, or the orphan file is
    // unrecoverable. The recomputed candidates alone are NOT the record this
    // leg owes: the config no longer answers for a routed Project's board
    // file, so an earlier attempt's capture -- the routed file the recompute
    // will never name -- is merged in rather than overwritten.
    if removed || had_tasks {
        let owed_files = if removed {
            board_files.clone()
        } else {
            let marked_before = storage
                .pending_project_tasks_deletions()
                .map_err(|e| format!(
                    "The Project's Task cleanup could not start because its pending records \
                     could not be read; nothing was deleted: {e}"
                ))?
                .into_iter()
                .find(|record| record.project_id == id);
            let mut owed_files = board_files.clone();
            if let Some(record) = marked_before {
                for file in record.board_files {
                    if !owed_files.contains(&file) {
                        owed_files.push(file);
                    }
                }
            }
            owed_files
        };
        if !removed {
            storage
                .mark_project_tasks_deletion_pending(id, &owed_files)
                .map_err(|e| format!(
                    "The Project's Task records could not be cleaned up because their pending \
                     cleanup record could not be written; nothing was deleted: {e}"
                ))?;
        }
        if let Err(error) = storage.delete_task_files(&owed_files) {
            return Err(format!(
                "The Project's Task records could not be removed, and their cleanup remains \
                 pending; the next start finishes it: {error}"
            ));
        }
        tasks_w.retain(|_, task| task.project_id != id);
        storage
            .clear_project_tasks_deletion_pending(id)
            .map_err(|e| {
                format!(
                "The Project's Task records are gone from disk and the board, but its pending Task \
                 cleanup record remains: {e}"
            )
            })?;
    }

    if removed {
        // The Conversation cleanup record was already persisted above; this
        // is where it is fulfilled. A Project whose removal committed earlier
        // (a retried deletion) reaches this call with `removed == false` and
        // leaves its Conversation leg to the restart's reconciliation; the
        // record -- present exactly while an attempt is unfinished -- covers
        // that leg, and this call would only re-derive the same guarded
        // removal the first attempt already proved.
        storage.delete_project_conversation(id).map_err(|e| {
            format!("Project was deleted, but Conversation cleanup is pending: {e}")
        })?;
        storage
            .clear_project_conversation_deletion_pending(id)
            .map_err(|e| {
                format!(
                    "Project was deleted and Conversation cleanup completed, but its pending \
                 cleanup record remains: {e}"
                )
            })?;
    }

    drop(tasks_w);
    // `true` means something durable left: the Project, or its Task records
    // (whose Project can already be absent from an earlier committed
    // attempt). A later retry of exactly this Project therefore answers
    // `false` -- that is the retry having nothing left to do, not a claim
    // that the first answer was wrong.
    Ok(removed || had_tasks)
}
#[cfg(test)]
mod delete_junction_test_support {
    //! A rendezvous a deletion test can string up mid-transaction.
    //!
    //! `delete_project_committed` runs the deletion purely asynchronously and
    //! the window between its durable Task read and its commit is exactly the
    //! interleaving a concurrent Task creation needs; production code has no
    //! seam there, so these two helpers are how a `#[tokio::test]` arms the
    //! hook instead: arm one junction, drive the deletion to it, race a Task
    //! creation against the pinned refusal, then release. One junction purges
    //! itself when taken, so a leftover arming cannot stale the next test.

    use tokio::sync::mpsc;
    use uuid::Uuid;

    // The junction holds the Project id it was armed for, so a deletion of any
    // other Project (a parallel non-junction test) passes straight through a
    // foreign arming instead of eating it.
    struct ArmedJunction {
        project_id: Uuid,
        arrived: mpsc::UnboundedSender<()>,
        release: std::sync::Mutex<mpsc::UnboundedReceiver<()>>,
    }

    static JUNCTION: std::sync::Mutex<Option<ArmedJunction>> = std::sync::Mutex::new(None);

    /// Cross-test serialization: the junction is process-global, so tests
    /// driving it must not race each other's arming. The guard is held for
    /// the duration of the test body.
    pub async fn serial() -> tokio::sync::OwnedMutexGuard<()> {
        static SERIALIZER: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
            std::sync::OnceLock::new();
        SERIALIZER
            .get_or_init(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
            .lock_owned()
            .await
    }
    /// Arm the junction. The two channels are used in the deletion's own task:
    /// `arrived` from the arm point, and the `deletion proceeds` release from
    /// the test's task.
    pub fn arm(
        project_id: Uuid,
    ) -> (
        mpsc::UnboundedReceiver<()>,
        mpsc::UnboundedSender<()>,
    ) {
        let (arrived_tx, arrived_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = mpsc::unbounded_channel();
        *JUNCTION.lock().unwrap() = Some(ArmedJunction {
            project_id,
            arrived: arrived_tx,
            release: std::sync::Mutex::new(release_rx),
        });
        (arrived_rx, release_tx)
    }

    /// The site inside the deletion, a no-op when no junction is armed *for
    /// this Project* — a deletion of any other Project passes through, so a
    /// concurrent non-junction test can never eat an armed junction. Taking
    /// it un-arms, so a second armed test will not wait here forever.
    pub async fn wait(project_id: Uuid) {
        const JUNCTION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
        let junction = JUNCTION.lock().unwrap().take_if(|junction| {
            junction.project_id == project_id
        });
        let ArmedJunction { arrived, release, .. } = match junction {
            Some(junction) => junction,
            None => return,
        };
        let _ = arrived.send(());
        let mut release = release
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match tokio::time::timeout(JUNCTION_DEADLINE, release.recv()).await {
            Ok(_) => {}
            Err(_) => panic!(
                "a junction test armed the deletion and never released it; the deletion sat \
                 at the junction for {JUNCTION_DEADLINE:?} (this deadline is the hang guard, \
                 not the behavior under test)"
            ),
        }
    }
}

#[cfg(test)]
async fn delete_junction(project_id: Uuid) {
    delete_junction_test_support::wait(project_id).await;
}

/// The live ownership a deletion refuses to outrun: an agent running for the
/// Task, or a pull request being opened for it. Shared by the durable refusal
/// pass and the commit-window re-check, so the two can never disagree.
async fn live_work_refusal(
    task: &Task,
    running: Option<&dyn crate::lifecycle::ExecutionOwnership>,
    deliveries: Option<&crate::commands::human_review::DeliveriesInFlight>,
) -> Option<String> {
    if let Some(running) = running {
        if running.is_task_running(task.id).await {
            return Some(format!(
                "an agent is running for task {:?} ({}); stop it before deleting the \
                 Project, because its working directory is one of this Project's \
                 checkouts",
                task.title, task.id
            ));
        }
    }
    if let Some(deliveries) = deliveries {
        if deliveries.contains(task.id) {
            return Some(format!(
                "a pull request is being opened for task {:?} ({}); wait for it to finish \
                 before deleting the Project",
                task.title, task.id
            ));
        }
    }
    None
}

/// The refusal a Project deletion answers with when one Task's durable work
/// is still outstanding: the Git recovery refs and the checkout are untouched.
fn project_task_refusal(task: &Task) -> Option<String> {
    let message = crate::lifecycle::project_deletion_refusal(task)?;
    Some(format!(
        "This Project cannot be deleted: {message}. Deleting its Task records would strand \
         durable work, so the Git recovery refs and the checkout are untouched."
    ))
}

#[tauri::command]
pub async fn update_project(
    state: tauri::State<'_, crate::AppState>,
    id: String,
    name: Option<String>,
    repository_id: Option<String>,
    agent_type: Option<AgentType>,
) -> Result<Option<Project>, String> {
    let id = Uuid::parse_str(&id).map_err(|e| e.to_string())?;
    // A `repository_id` change re-derives the Project's routes: hold the
    // Project's lifecycle gate shared for the persist so a concurrent deletion
    // captures and removes this Project's board files by one coherent route.
    let _gate = state.task.project_gate.read(id).await;
    update_project_committed(&state.project.projects, &state.storage, id, name, repository_id, agent_type).await
}

/// Core of [`update_project`]; see [`create_project_committed`] for why the
/// map is cloned first. A failed persist never leaves memory holding an
/// update the caller was told did not apply.
async fn update_project_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    id: Uuid,
    name: Option<String>,
    repository_id: Option<String>,
    agent_type: Option<AgentType>,
) -> Result<Option<Project>, String> {
    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();

    if let Some(project) = proposed.get_mut(&id) {
        if let Some(name) = name {
            project.name = name;
        }
        if let Some(repository_id) = repository_id {
            project.repository_id = Uuid::parse_str(&repository_id).ok();
        }
        if let Some(agent_type) = agent_type {
            project.agent_type = agent_type.clone();
            project.agent_config = get_default_agent_config(&agent_type);
        }
        project.updated_at = chrono::Utc::now();
        let result = project.clone();

        try_persist_projects(storage, &proposed)
            .map_err(|e| format!("Failed to persist project update: {e}"))?;

        *projects_w = proposed;
        Ok(Some(result))
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub async fn attach_project_to_workspace(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    workspace_id: String,
) -> Result<Project, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;

    attach_project_to_workspace_committed(
        &state.project.projects,
        &state.workspace.registry,
        &state.storage,
        project_id,
        workspace_id,
    )
    .await
}

/// Core of [`attach_project_to_workspace`]; see [`create_project_committed`]
/// for why the map is cloned first. Refuses a project that is already in a
/// workspace rather than silently re-parenting it -- moving between
/// workspaces is an ambiguous action a caller should express as an explicit
/// detach followed by an attach.
///
/// The workspace's existence is checked while both locks are held, and they
/// stay held through the persist: checking the registry and then releasing
/// it would let `delete_workspace` remove the workspace before the
/// attachment lands, leaving the project pointing at nothing. Locks are taken
/// in the order every path that needs both uses -- projects, then the
/// workspace registry -- so this cannot deadlock against `delete_workspace`.
async fn attach_project_to_workspace_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    registry: &RwLock<WorkspaceRegistry>,
    storage: &Storage,
    project_id: Uuid,
    workspace_id: Uuid,
) -> Result<Project, String> {
    let mut projects_w = projects.write().await;
    let registry_r = registry.read().await;
    if registry_r.get(&workspace_id).is_none() {
        return Err("Workspace not found".to_string());
    }
    let mut proposed = projects_w.clone();

    let project = proposed
        .get_mut(&project_id)
        .ok_or_else(|| "Project not found".to_string())?;
    if !matches!(project.scope, ProjectScope::Standalone) {
        return Err("Project already belongs to a workspace; detach it first".to_string());
    }
    project.scope = ProjectScope::InWorkspace { workspace_id };
    project.updated_at = chrono::Utc::now();
    let result = project.clone();

    try_persist_projects(storage, &proposed)
        .map_err(|e| format!("Failed to persist workspace attachment: {e}"))?;

    *projects_w = proposed;
    Ok(result)
}

#[tauri::command]
pub async fn detach_project_from_workspace(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Project, String> {
    let project_id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    detach_project_from_workspace_committed(&state.project.projects, &state.storage, project_id).await
}

/// Core of [`detach_project_from_workspace`]; see [`create_project_committed`]
/// for why the map is cloned first. Refuses a project that is already
/// standalone so a caller cannot mistake a no-op for a successful detach.
async fn detach_project_from_workspace_committed(
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    project_id: Uuid,
) -> Result<Project, String> {
    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();

    let project = proposed
        .get_mut(&project_id)
        .ok_or_else(|| "Project not found".to_string())?;
    if matches!(project.scope, ProjectScope::Standalone) {
        return Err("Project is not attached to a workspace".to_string());
    }
    project.scope = ProjectScope::Standalone;
    project.updated_at = chrono::Utc::now();
    let result = project.clone();

    try_persist_projects(storage, &proposed)
        .map_err(|e| format!("Failed to persist workspace detachment: {e}"))?;

    *projects_w = proposed;
    Ok(result)
}

/// Get the working directory path for a project by looking up its repository
#[tauri::command]
pub async fn get_project_path(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<Option<String>, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;

    let projects = state.project.projects.read().await;
    let Some(project) = projects.get(&id) else {
        return Ok(None);
    };
    let repositories = state.repository.repositories.read().await;
    Ok(project.repository_path(&repositories))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::{AppPaths, StateLocation};
    use crate::config::storage::AppConfig;
    use crate::domain::ProjectScope;
    use tempfile::TempDir;

    fn create_test_storage() -> (Storage, TempDir) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let root = temp_dir.path();
        std::fs::create_dir_all(root.join("config")).expect("Failed to create config dir");
        std::fs::create_dir_all(root.join("data")).expect("Failed to create data dir");

        let storage = Storage::with_paths(AppPaths::with_roots(
            root.join("config"),
            root.join("data"),
            root.join("cache"),
            root.join("runtime"),
        ));

        (storage, temp_dir)
    }

    /// The shared Task map a deletion commits against.
    fn board(tasks: Vec<Task>) -> crate::lifecycle::Tasks {
        std::sync::Arc::new(RwLock::new(tasks.into_iter().map(|t| (t.id, t)).collect()))
    }

    /// A Task with nothing outstanding: Done, no checkout, and an integration
    /// record that already finished. An eligible-Project deletion removes it.
    fn board_task(project_id: Uuid) -> Task {
        let mut task = crate::test_helpers::create_test_task_full(
            "Holdings done",
            project_id,
            crate::domain::TaskStatus::Done,
            0,
        );
        task.local_integration = Some(finished_integration());
        task
    }

    fn finished_integration() -> crate::domain::LocalIntegration {
        local_integration(crate::domain::IntegrationState::Integrated)
    }

    fn pending_integration() -> crate::domain::LocalIntegration {
        local_integration(crate::domain::IntegrationState::Pending)
    }

    fn local_integration(
        state: crate::domain::IntegrationState,
    ) -> crate::domain::LocalIntegration {
        crate::domain::LocalIntegration {
            state,
            target_branch: "main".to_string(),
            target_old_oid: "0000000000000000000000000000000000000000".to_string(),
            approved_tip: "1111111111111111111111111111111111111111".to_string(),
            result_oid: "2222222222222222222222222222222222222222".to_string(),
            strategy: crate::domain::IntegrationStrategy::FastForward,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: false,
        }
    }

    fn pending_restack() -> crate::domain::PendingRepublish {
        crate::domain::PendingRepublish {
            parent_branch: "parent-branch".to_string(),
            parent_pr: 7,
            pr_number: None,
            default_branch: "main".to_string(),
            fork_point: "0000000000000000000000000000000000000000".to_string(),
            previous_tip: "1111111111111111111111111111111111111111".to_string(),
            onto: "4444444444444444444444444444444444444444".to_string(),
            rewritten_tip: None,
        }
    }

    /// Seed the project's durable Task records and the shared board holding
    /// them.
    fn seed_board(
        project_id: Uuid,
        tasks_storage: &Storage,
        tasks: &[Task],
    ) -> crate::lifecycle::Tasks {
        tasks_storage.save_project_tasks(project_id, tasks).unwrap();
        board(tasks.to_vec())
    }

    fn board_tasks_file(project_id: Uuid, tasks_storage: &Storage) -> std::path::PathBuf {
        // Tests do not route a project to a state directory of its own, so
        // the board lives in the shared (legacy) tasks directory.
        tasks_storage
            .paths()
            .config_dir()
            .join("tasks")
            .join(format!("{project_id}.toml"))
    }

    /// An [`ExecutionOwnership`] double that answers for one task.
    struct FakeRuns {
        running: Vec<Uuid>,
    }

    impl FakeRuns {
        fn running(task_id: Uuid) -> Self {
            Self {
                running: vec![task_id],
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::lifecycle::ExecutionOwnership for FakeRuns {
        async fn is_task_running(&self, task_id: Uuid) -> bool {
            self.running.contains(&task_id)
        }
    }

    fn make_test_project(id: Uuid) -> Project {
        Project {
            id,
            name: "Test Project".to_string(),
            repository_id: None,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
            base: None,
            agent_type: AgentType::ClaudeCode,
            agent_config: get_default_agent_config(&AgentType::ClaudeCode),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn try_persist_projects_refuses_to_reduce_a_config_it_cannot_fully_parse() {
        // End-to-end counterpart to the storage-level test: the production
        // writer must surface the refusal rather than committing a
        // partially-recovered config over the real one. Built by serializing a
        // real config and invalidating one enum variant, so every other
        // section keeps the shape the app actually writes.
        let (storage, _temp) = create_test_storage();

        let mut seeded = AppConfig::default();
        seeded.repositories.insert(
            "11111111-1111-1111-1111-111111111111".to_string(),
            crate::domain::Repository {
                id: Uuid::nil(),
                local_path: "/home/user/repo".to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let valid = toml::to_string_pretty(&seeded).expect("serialize fixture");
        let poisoned = valid.replace("placement = \"managed\"", "placement = \"shared_root\"");
        assert_ne!(poisoned, valid, "fixture must invalidate the placement variant");
        std::fs::write(storage.paths().config_file(), &poisoned).expect("write fixture");

        let mut projects = HashMap::new();
        let id = Uuid::new_v4();
        projects.insert(id, make_test_project(id));

        let result = try_persist_projects(&storage, &projects);

        assert!(
            result.is_err(),
            "persisting must fail rather than write a config rebuilt from defaults"
        );
        let on_disk =
            std::fs::read_to_string(storage.paths().config_file()).expect("config still readable");
        assert_eq!(on_disk, poisoned, "the config on disk must be untouched");
        assert!(
            on_disk.contains("/home/user/repo"),
            "the repository section must survive the refused write"
        );
    }

    #[test]
    fn try_persist_projects_missing_config_file_initializes_from_defaults() {
        // No config.toml exists yet — this must behave exactly like today:
        // an `Ok` default config that the new projects are written into.
        let (storage, _temp) = create_test_storage();

        let mut projects = HashMap::new();
        let id = Uuid::new_v4();
        projects.insert(id, make_test_project(id));

        try_persist_projects(&storage, &projects).expect("should succeed against a missing config");

        let loaded = storage.load_config().expect("should load the freshly written config");
        assert_eq!(loaded.projects.len(), 1);
        assert!(loaded.projects.contains_key(&id.to_string()));
        // Everything else should still be plain defaults.
        assert_eq!(loaded.ui_preferences.theme, AppConfig::default().ui_preferences.theme);
    }

    #[cfg(unix)]
    #[test]
    fn try_persist_projects_propagates_read_failure_without_destroying_config() {
        use std::os::unix::fs::PermissionsExt;

        if crate::ipc::server::current_uid() == 0 {
            return; // root ignores permission bits, so chmod 0o000 would not force a read failure
        }

        let (storage, _temp) = create_test_storage();

        // Seed a real config with data that must survive a failed persist.
        let mut seed = AppConfig::default();
        seed.jj_config.user_name = Some("Keep Me".to_string());
        storage.save_config(&seed).expect("seed config");

        let config_path = storage.paths().config_file();
        let original_bytes = std::fs::read(&config_path).unwrap();

        // Force load_config() to return Err by making the existing file unreadable.
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut projects = HashMap::new();
        let id = Uuid::new_v4();
        projects.insert(id, make_test_project(id));

        let result = try_persist_projects(&storage, &projects);

        // Restore permissions so the file can be inspected and cleaned up.
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(result.is_err(), "a read failure must not be treated as success");
        let on_disk = std::fs::read(&config_path).unwrap();
        assert_eq!(on_disk, original_bytes, "config on disk must be untouched by a failed persist");
    }

    #[test]
    fn concurrent_project_and_repository_persists_never_lose_each_other() {
        // `config.toml` is a single file holding both sections, but a project
        // save and a repository save are serialized by two *different*
        // in-memory locks. Each helper replaces only its own section yet
        // rewrites the whole struct, so before `Storage::update_config` two
        // overlapping saves could both read the same config, both report
        // success, and leave whichever wrote second having reverted the
        // other's section. Every write is atomic, so nothing is ever torn —
        // the update is simply lost, which is why this needs a test rather
        // than showing up as corruption.
        //
        // Both sides start from a shared barrier so their read-modify-write
        // windows actually overlap. The assertion runs after *every* round,
        // not just at the end: each helper persists the full accumulated map,
        // so a section lost in one round would be rewritten by the next
        // round's save and never observed by a single final check.
        use crate::commands::repository::try_persist_repositories;
        use crate::domain::Repository;

        let (storage, _temp) = create_test_storage();

        const ROUNDS: usize = 40;
        let mut projects: HashMap<Uuid, Project> = HashMap::new();
        let mut repositories: HashMap<Uuid, Repository> = HashMap::new();

        for round in 1..=ROUNDS {
            let project_id = Uuid::new_v4();
            projects.insert(project_id, make_test_project(project_id));

            let repository_id = Uuid::new_v4();
            repositories.insert(
                repository_id,
                Repository {
                    id: repository_id,
                    local_path: format!("/repo/{round}"),
                    remote_url: None,
                    remote_type: None,
                    created_at: chrono::Utc::now(),
                },
            );

            let barrier = Arc::new(std::sync::Barrier::new(2));

            let project_side = {
                let storage = storage.clone();
                let projects = projects.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    try_persist_projects(&storage, &projects)
                })
            };

            let repository_side = {
                let storage = storage.clone();
                let repositories = repositories.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    try_persist_repositories(&storage, &repositories)
                })
            };

            project_side
                .join()
                .expect("project persist thread should not panic")
                .expect("project persist should succeed");
            repository_side
                .join()
                .expect("repository persist thread should not panic")
                .expect("repository persist should succeed");

            let on_disk = storage.load_config().expect("config should be readable");
            assert_eq!(
                on_disk.projects.len(),
                round,
                "round {round}: a concurrent repository save reverted the projects section"
            );
            assert_eq!(
                on_disk.repositories.len(),
                round,
                "round {round}: a concurrent project save reverted the repositories section"
            );
        }
    }

    /// Make `config.toml` exist but unreadable, so `try_persist_projects`
    /// fails deep inside its own `load_config()` call — the same forcing
    /// technique the read-failure test above uses, reused here to prove the
    /// *committed helpers* leave memory untouched on that failure, not just
    /// that the persistence function itself reports it.
    #[cfg(unix)]
    fn make_storage_with_unreadable_config() -> (Storage, TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let (storage, temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).expect("seed config");
        std::fs::set_permissions(storage.paths().config_file(), std::fs::Permissions::from_mode(0o000)).unwrap();
        (storage, temp)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn create_project_leaves_memory_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::new());
        let new_project = make_test_project(Uuid::new_v4());

        let result = create_project_committed(&projects, &storage, new_project.clone()).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert!(
            projects.read().await.is_empty(),
            "memory must not contain a project disk never recorded"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn delete_project_leaves_memory_and_task_files_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let conversation = storage.create_primary_conversation(id).unwrap();
        let pointer = storage.paths().primary_conversation_file(id);
        let document = storage.paths().conversation_file(conversation.id);
        let task = board_task(id);
        let tasks_file = board_tasks_file(id, &storage);
        storage.save_project_tasks(id, std::slice::from_ref(&task)).unwrap();
        let tasks = board(vec![task]);
        let projects: RwLock<HashMap<Uuid, Project>> =
            RwLock::new(HashMap::from([(id, make_test_project(id))]));

        let result = delete_project_committed(&projects, &tasks, &storage, None, None, id).await;

        assert!(
            result.is_err(),
            "a persistence failure must be reported, not swallowed"
        );
        assert!(
            projects.read().await.contains_key(&id),
            "memory must still contain the project a failed deletion could not persist"
        );
        assert!(pointer.exists());
        assert!(document.exists());
        // The Task records survive an uncommitted deletion untouched; no
        // retry record was left behind either, because cleanup never began.
        assert!(tasks_file.exists());
        assert!(!storage
            .paths()
            .pending_project_tasks_deletion_file(id)
            .exists());
        assert!(
            tasks.read().await.values().any(|t| t.project_id == id),
            "the board must keep the Task records of a Project that still exists"
        );
    }

    #[tokio::test]
    async fn deleting_project_cleans_its_primary_conversation_after_config_commit() {
        let (storage, _temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).unwrap();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        let conversation = storage.create_primary_conversation(id).unwrap();
        let document = storage.paths().conversation_file(conversation.id);
        let pointer = storage.paths().primary_conversation_file(id);
        let done = board_task(id);
        let backlog = crate::test_helpers::create_test_task_full(
            "Backlog card",
            id,
            crate::domain::TaskStatus::Backlog,
            0,
        );
        storage
            .save_project_tasks(id, &[done.clone(), backlog.clone()])
            .unwrap();
        let tasks = board(vec![done, backlog]);
        let projects = RwLock::new(HashMap::from([(id, project)]));

        // Nothing on the board owes the user anything, so deletion proceeds:
        // an eligible Project is deleted together with its Task records, and
        // the answer is success.
        assert!(
            delete_project_committed(&projects, &tasks, &storage, None, None, id)
                .await
                .unwrap()
        );

        assert!(!projects.read().await.contains_key(&id));
        assert!(!pointer.exists());
        assert!(!document.exists());
        assert!(!storage
            .paths()
            .pending_project_conversation_deletion_file(id)
            .exists());
        // No orphan Task is left in memory or on disk.
        assert!(
            tasks.read().await.values().all(|t| t.project_id != id),
            "memory must not keep Tasks of a deleted Project"
        );
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 0);
        assert!(!storage
            .paths()
            .pending_project_tasks_deletion_file(id)
            .exists());
        // A restart reads exactly the removed state back.
        let restarted = storage.clone();
        assert_eq!(restarted.load_project_tasks(id).unwrap().len(), 0);
        assert!(!restarted
            .load_config_strict()
            .unwrap()
            .projects
            .contains_key(&id.to_string()));
    }

    #[tokio::test]
    async fn committed_project_deletion_reports_cleanup_failure_and_keeps_restart_record() {
        let (storage, _temp) = create_test_storage();
        storage.save_config(&AppConfig::default()).unwrap();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        let conversation = storage.create_primary_conversation(id).unwrap();
        let pointer = storage.paths().primary_conversation_file(id);
        let document = storage.paths().conversation_file(conversation.id);
        std::fs::write(&document, "malformed Conversation").unwrap();
        let tasks_file = storage
            .paths()
            .pending_project_tasks_deletion_file(id)
            .with_file_name(format!("{id}.toml"));
        let done = board_task(id);
        storage.save_project_tasks(id, std::slice::from_ref(&done)).unwrap();
        let tasks = board(vec![done]);
        let projects = RwLock::new(HashMap::from([(id, project)]));

        let result = delete_project_committed(&projects, &tasks, &storage, None, None, id).await;

        assert!(
            result.is_err(),
            "cleanup failure must not be reported as success"
        );
        assert!(!projects.read().await.contains_key(&id));
        assert!(pointer.exists());
        assert_eq!(
            std::fs::read_to_string(document).unwrap(),
            "malformed Conversation"
        );
        assert!(storage
            .paths()
            .pending_project_conversation_deletion_file(id)
            .exists());
        // The Task cleanup, which runs before the Conversation cleanup,
        // succeeded: no orphan Task in memory or on disk, and no Task retry
        // record left behind.
        assert!(tasks.read().await.values().all(|t| t.project_id != id));
        assert!(!tasks_file.exists());
        assert!(!storage
            .paths()
            .pending_project_tasks_deletion_file(id)
            .exists());
    }

    #[tokio::test]
    async fn live_project_conversation_refuses_deletion_before_persistence() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig {
            projects: HashMap::from([(id.to_string(), project)]),
            ..AppConfig::default()
        }).unwrap();
        let conversation = storage.create_primary_conversation(id).unwrap();
        let pointer = storage.paths().primary_conversation_file(id);
        let document = storage.paths().conversation_file(conversation.id);

        let result = lock_project_conversation_for_deletion(
            &crate::commands::conversation::ConversationState::new(),
            &storage,
            id,
            |_| true,
        ).await;

        assert!(result.is_err());
        assert!(pointer.exists());
        assert!(document.exists());
        assert!(storage.load_config_strict().unwrap().projects.contains_key(&id.to_string()));
        assert!(!storage.paths().pending_project_conversation_deletion_file(id).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn update_project_leaves_memory_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let original_name = existing.name.clone();
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));

        let result = update_project_committed(
            &projects, &storage, id, Some("New Name".to_string()), None, None,
        ).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert_eq!(
            projects.read().await.get(&id).unwrap().name,
            original_name,
            "memory must not contain an update disk never recorded"
        );
    }

    /// A registry, backed by a tempdir, holding one real workspace.
    struct RegisteredWorkspace {
        registry: RwLock<WorkspaceRegistry>,
        id: Uuid,
        _registry_dir: TempDir,
        _root: TempDir,
    }

    fn registered_workspace() -> RegisteredWorkspace {
        let root = TempDir::new().expect("workspace root tempdir");
        let registry_dir = TempDir::new().expect("registry tempdir");
        let mut registry = WorkspaceRegistry::load_from(registry_dir.path().join("workspaces.toml"))
            .expect("a missing registry file should load empty");
        let workspace = crate::domain::Workspace::new(
            "ws".to_string(),
            crate::domain::WorkspaceRoot::try_new(root.path()).expect("real dir must validate"),
        );
        let id = workspace.id;
        registry.upsert(workspace).expect("upsert should succeed");
        RegisteredWorkspace {
            registry: RwLock::new(registry),
            id,
            _registry_dir: registry_dir,
            _root: root,
        }
    }

    #[tokio::test]
    async fn attach_project_to_workspace_committed_attaches_a_standalone_project() {
        let (storage, _temp) = create_test_storage();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));
        let ws = registered_workspace();
        let workspace_id = ws.id;

        let result = attach_project_to_workspace_committed(&projects, &ws.registry, &storage, id, workspace_id)
            .await
            .expect("attaching a standalone project should succeed");

        assert_eq!(result.scope.workspace_id(), Some(workspace_id));
        assert_eq!(
            projects.read().await.get(&id).unwrap().scope.workspace_id(),
            Some(workspace_id),
            "the attachment must be reflected in memory"
        );
        let persisted = storage.load_config().expect("config should load");
        let persisted_project = persisted
            .projects
            .get(&id.to_string())
            .expect("project should still be persisted");
        assert_eq!(
            persisted_project.scope.workspace_id(),
            Some(workspace_id),
            "the attachment must be persisted"
        );
    }

    #[tokio::test]
    async fn attach_project_to_workspace_committed_refuses_a_project_already_in_a_workspace() {
        let (storage, _temp) = create_test_storage();
        let mut existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let original_workspace_id = Uuid::new_v4();
        existing.scope = ProjectScope::InWorkspace { workspace_id: original_workspace_id };
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));
        let ws = registered_workspace();

        let result =
            attach_project_to_workspace_committed(&projects, &ws.registry, &storage, id, ws.id).await;

        assert!(result.is_err(), "re-parenting an already-attached project must be refused");
        assert_eq!(
            projects.read().await.get(&id).unwrap().scope.workspace_id(),
            Some(original_workspace_id),
            "the refused attach must not change the existing membership"
        );
    }

    #[tokio::test]
    async fn attach_project_to_workspace_committed_refuses_a_missing_project() {
        let (storage, _temp) = create_test_storage();
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::new());
        let ws = registered_workspace();

        let result =
            attach_project_to_workspace_committed(&projects, &ws.registry, &storage, Uuid::new_v4(), ws.id)
                .await;

        assert!(result.is_err(), "attaching a project that does not exist must be refused");
    }

    #[tokio::test]
    async fn attach_project_to_workspace_committed_refuses_a_missing_workspace() {
        let (storage, _temp) = create_test_storage();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));
        let ws = registered_workspace();

        let result =
            attach_project_to_workspace_committed(&projects, &ws.registry, &storage, id, Uuid::new_v4())
                .await;

        assert_eq!(result.unwrap_err(), "Workspace not found");
        assert_eq!(
            projects.read().await.get(&id).unwrap().scope.workspace_id(),
            None,
            "a refused attach must not change memory"
        );
        let persisted = storage.load_config().expect("config should load");
        assert!(
            persisted
                .projects
                .get(&id.to_string())
                .is_none_or(|p| p.scope.workspace_id().is_none()),
            "a refused attach must not be persisted"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn attach_project_to_workspace_leaves_memory_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));
        // A real workspace, so the failure under test is the persist and not
        // the existence check that precedes it.
        let ws = registered_workspace();

        let result =
            attach_project_to_workspace_committed(&projects, &ws.registry, &storage, id, ws.id).await;

        let error = result.expect_err("a persistence failure must be reported, not swallowed");
        assert!(
            error.starts_with("Failed to persist workspace attachment"),
            "the refusal must come from the persist, got {error:?}"
        );
        assert_eq!(
            projects.read().await.get(&id).unwrap().scope.workspace_id(),
            None,
            "memory must not contain an attachment disk never recorded"
        );
    }

    #[tokio::test]
    async fn detach_project_from_workspace_committed_detaches_a_member_project() {
        let (storage, _temp) = create_test_storage();
        let mut existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        existing.scope = ProjectScope::InWorkspace { workspace_id: Uuid::new_v4() };
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));

        let result = detach_project_from_workspace_committed(&projects, &storage, id)
            .await
            .expect("detaching a member project should succeed");

        assert_eq!(result.scope.workspace_id(), None);
        let persisted = storage.load_config().expect("config should load");
        let persisted_project = persisted
            .projects
            .get(&id.to_string())
            .expect("project should still be persisted");
        assert_eq!(
            persisted_project.scope.workspace_id(),
            None,
            "the detachment must be persisted"
        );
    }

    #[tokio::test]
    async fn detach_project_from_workspace_committed_refuses_an_already_standalone_project() {
        let (storage, _temp) = create_test_storage();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));

        let result = detach_project_from_workspace_committed(&projects, &storage, id).await;

        assert!(result.is_err(), "detaching an already-standalone project must be refused");
    }

    #[tokio::test]
    async fn detach_project_from_workspace_committed_refuses_a_missing_project() {
        let (storage, _temp) = create_test_storage();
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::new());

        let result =
            detach_project_from_workspace_committed(&projects, &storage, Uuid::new_v4()).await;

        assert!(result.is_err(), "detaching a project that does not exist must be refused");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn detach_project_from_workspace_leaves_memory_unchanged_when_persistence_fails() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = make_storage_with_unreadable_config();
        let mut existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let workspace_id = Uuid::new_v4();
        existing.scope = ProjectScope::InWorkspace { workspace_id };
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));

        let result = detach_project_from_workspace_committed(&projects, &storage, id).await;

        assert!(result.is_err(), "a persistence failure must be reported, not swallowed");
        assert_eq!(
            projects.read().await.get(&id).unwrap().scope.workspace_id(),
            Some(workspace_id),
            "memory must not contain a detachment disk never recorded"
        );
    }

    /// Recovery from a lost Workspace, end to end at the command layer: the
    /// registry is corrupted and quarantined, so the Project's membership no
    /// longer resolves. Attaching elsewhere is still refused -- a dangling
    /// membership is not silently re-parented -- while detaching succeeds
    /// without the Workspace, persists, and makes the Project attachable to a
    /// Workspace that does exist.
    #[tokio::test]
    async fn a_project_whose_workspace_was_lost_can_be_detached_and_attached_again() {
        let (storage, _temp) = create_test_storage();
        let lost = registered_workspace();
        let existing = make_test_project(Uuid::new_v4());
        let id = existing.id;
        let projects: RwLock<HashMap<Uuid, Project>> = RwLock::new(HashMap::from([(id, existing)]));
        attach_project_to_workspace_committed(&projects, &lost.registry, &storage, id, lost.id)
            .await
            .expect("attaching to a registered workspace should succeed");

        let registry_file = lost._registry_dir.path().join("workspaces.toml");
        std::fs::write(&registry_file, "this is [[not toml").unwrap();
        let reloaded = WorkspaceRegistry::load_from(registry_file)
            .expect("a corrupt registry is quarantined, not fatal");
        assert!(reloaded.get(&lost.id).is_none(), "the workspace must now be missing");
        let survivor = registered_workspace();

        let refused =
            attach_project_to_workspace_committed(&projects, &survivor.registry, &storage, id, survivor.id)
                .await;
        assert!(refused.is_err(), "a dangling membership must not be silently re-parented");

        let detached = detach_project_from_workspace_committed(&projects, &storage, id)
            .await
            .expect("detaching must not require the lost workspace to exist");
        assert_eq!(detached.scope.workspace_id(), None);
        assert_eq!(
            storage.load_config().unwrap().projects[&id.to_string()].scope.workspace_id(),
            None,
            "the detachment must be persisted"
        );

        let reattached =
            attach_project_to_workspace_committed(&projects, &survivor.registry, &storage, id, survivor.id)
                .await
                .expect("a detached project is attachable again");
        assert_eq!(reattached.scope.workspace_id(), Some(survivor.id));
    }
    // ------------------------------------------------------------------
    // Safe Project deletion: refusals, durables and concurrency (issue #193)
    // ------------------------------------------------------------------

    /// A deletion refused on durable data-integrity grounds, before anything
    /// was written: the error names the forbidden deletion and no retry
    /// record is left behind.
    fn refused_obligation(
        result: std::result::Result<bool, String>,
        project_id: Uuid,
        storage: &Storage,
    ) {
        let error = result.expect_err("deletion must be refused");
        assert!(
            !error.contains("pending") && error.contains("cannot be deleted"),
            "the refusal must name the forbidden deletion: {error:?}"
        );
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(project_id)
                .exists(),
            "no Task cleanup record is owed for a deletion that never began"
        );
        assert!(
            !storage
                .paths()
                .pending_project_conversation_deletion_file(project_id)
                .exists(),
            "no Conversation cleanup record is owed for a deletion that never began"
        );
    }

    /// A refusal, whatever its reason: nothing was committed and no retry
    /// record is left for a later start.
    fn refused(result: std::result::Result<bool, String>, project_id: Uuid, storage: &Storage) {
        let _error = result.expect_err("deletion must be refused");
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(project_id)
                .exists(),
            "no Task cleanup record is owed for a deletion that never began"
        );
        assert!(
            !storage
                .paths()
                .pending_project_conversation_deletion_file(project_id)
                .exists(),
            "no Conversation cleanup record is owed for a deletion that never began"
        );
    }

    #[tokio::test]
    async fn a_task_created_during_the_deletion_is_refused_with_the_project() {
        create_during_deletion_repro(true).await;
    }

    #[tokio::test]
    async fn a_plain_backlog_task_created_during_the_deletion_is_also_refused() {
        create_during_deletion_repro(false).await;
    }

    /// Race a Task `create` against a deletion held at its junction.
    ///
    /// The post-fix contract asserted here: the creation queues behind the
    /// deletion, is refused by its own "Project does not exist or was
    /// deleted" check once the removal has committed, and nothing durable is
    /// born that the user was told exists. On the pre-fix head this test
    /// fails with the observable race instead: the `create` answered
    /// `Ok(task)` mid-transaction, the removal that had already committed had
    /// not been seen by any check, and a Task owing durable work turned the
    /// post-commit board check into a refusal-after-commit with the Task
    /// cleanup record already on disk.
    async fn create_during_deletion_repro(owed: bool) {
        // The junction is process-global; tests using it run one at a time.
        let _serial = super::delete_junction_test_support::serial().await;

        let (storage, _temp) = create_test_storage();
        let id = Uuid::new_v4();
        let mut config = AppConfig::default();
        config.projects.insert(id.to_string(), make_test_project(id));
        storage.save_config(&config).unwrap();
        // A real (empty) board file: the deletion must dispose of exactly it,
        // and the racing creation must never touch the path again.
        seed_board(id, &storage, &[]);
        let tasks = board(Vec::new());
        let projects = Arc::new(RwLock::new(HashMap::from([(id, make_test_project(id))])));

        let (mut arrived, release_tx) = super::delete_junction_test_support::arm(id);

        // The deletion as `delete_project` drives it: the gate in write (the
        // Conversation lock would come first in the command, but that lock
        // guards Providers, not Task creation, and the race under test needs
        // no provider), the leases, then the commit.
        let gate = crate::lifecycle::ProjectLifecycleGate::new();
        let locks = crate::lifecycle::TaskLifecycleLocks::new();
        let deletion = {
            let projects = projects.clone();
            let tasks = tasks.clone();
            let storage = storage.clone();
            let gate = gate.clone();
            async move {
                let _gate = gate.write(id).await;
                let _leases = crate::lifecycle::hold_project_task_leases(&locks, &tasks, id)
                    .await
                    .expect("no lifecycle operation owns these tasks in this test");
                delete_project_committed(&projects, &tasks, &storage, None, None, id).await
            }
        };
        let del_task = tokio::spawn(deletion);

        // Hold at the junction: past the durable read, before the commit.
        arrived.recv().await.expect("a junction cannot close early");

        // A Task whose record lands mid-transaction: plain, or owing durable
        // work, per `owed`.
        let new = crate::domain::NewTask {
            id: Uuid::new_v4(),
            project_id: id,
            title: "Born during the deletion".into(),
            description: None,
            model: "default".to_string(),
            planning_mode: false,
            dependencies: Vec::new(),
            category: Default::default(),
            priority: Default::default(),
            complexity: Default::default(),
            impact: Default::default(),
            security_severity: Default::default(),
            github_issue_url: None,
            gitlab_issue_url: None,
            linear_ticket_id: None,
        };
        let create = {
            let projects = projects.clone();
            let tasks = tasks.clone();
            let storage = storage.clone();
            async move {
                crate::lifecycle::create(&gate, &tasks, &projects, &storage, id, move |existing| {
                    let mut task = crate::domain::Task::new_backlog(existing, new);
                    if owed {
                        task.local_integration = Some(pending_integration());
                        task.worktree_path = Some("/tmp/born-with-a-checkout".into());
                        task.branch_name = Some("slashit/born-with-a-checkout".into());
                    }
                    task
                })
                .await
            }
        };
        let create_task = tokio::spawn(create);

        // Release the deletion and let both sides settle.
        let _ = release_tx.send(());
        let (deleted, created) = tokio::join!(del_task, create_task);
        let deleted = deleted.expect("the deletion must not panic");
        let created = created.expect("the creation must not panic");

        assert!(
            created.is_err(),
            "a Task created during its Project's deletion must be refused, not born and then \
             vaporized (or deletion-refused without being told): creation answered \
             {created:?} while the deletion answered {deleted:?}"
        );
        assert!(
            matches!(&created, Err(message) if message.contains("does not exist or was deleted")),
            "the refusal must say the Project is gone: {created:?}"
        );
        assert!(
            matches!(deleted, Ok(true)),
            "the deletion itself must commit when nothing pre-existing owed work: {deleted:?}"
        );

        // Nothing of the born-then-refused Task survives anywhere, and the
        // records of the committed deletion are consistent: no marker, an
        // empty board, an empty durable record set.
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(id)
                .exists(),
            "the committed deletion must have cleared its Task cleanup record"
        );
        assert!(
            !storage
                .paths()
                .pending_project_conversation_deletion_file(id)
                .exists(),
            "the committed deletion must have cleared its Conversation cleanup record"
        );
        assert!(tasks.read().await.values().all(|t| t.project_id != id));
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_pending_integration_in_approved_human_review_refuses_deletion() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let mut task = board_task(id);
        task.status = crate::domain::TaskStatus::HumanReview;
        task.human_review
            .approve(Some("111".into()), chrono::Utc::now());
        task.local_integration = Some(pending_integration());
        let tasks = seed_board(id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));

        refused_obligation(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );
        // Everything survives the refusal, on disk as it was found.
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
        assert!(projects.read().await.contains_key(&id));
    }

    #[tokio::test]
    async fn a_pending_integration_in_an_unexpected_status_refuses_deletion() {
        // InProgress is a status without a Resume control. The Task record is
        // the only durable copy of the pending integration, so deletion is
        // refused anyway; this is the case `integration_unfinished_refusal`
        // would let past.
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let mut task = board_task(id);
        task.status = crate::domain::TaskStatus::InProgress;
        task.local_integration = Some(pending_integration());
        let tasks = seed_board(id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));

        refused_obligation(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_pending_restack_refuses_deletion() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let mut task = board_task(id);
        task.pending_republish = Some(pending_restack());
        let tasks = seed_board(id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));

        refused_obligation(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_active_agent_run_refuses_deletion() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let task = board_task(id);
        let tasks = seed_board(id, &storage, std::slice::from_ref(&task));
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        let runs = FakeRuns::running(task.id);

        refused(
            delete_project_committed(&projects, &tasks, &storage, Some(&runs), None, id).await,
            id,
            &storage,
        );
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_delivery_in_flight_refuses_deletion() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let task = board_task(id);
        let tasks = seed_board(id, &storage, std::slice::from_ref(&task));
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        let deliveries = crate::commands::human_review::DeliveriesInFlight::default();
        let _mark = deliveries.start(task.id); // held for the rest of the test

        refused(
            delete_project_committed(&projects, &tasks, &storage, None, Some(&deliveries), id)
                .await,
            id,
            &storage,
        );
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
    }

    /// A delivery marker raised while the deletion sits at the commit window:
    /// re-checked inside the transaction, before anything is written, so the
    /// answer is a clean refusal and every durable fact survives it.
    #[tokio::test]
    async fn a_delivery_marker_raised_in_the_commit_window_refuses_before_anything_is_written() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        let mut config = AppConfig::default();
        config.projects.insert(id.to_string(), make_test_project(id));
        storage.save_config(&config).unwrap();
        let task = board_task(id);
        let tasks = seed_board(id, &storage, std::slice::from_ref(&task));
        let projects = Arc::new(RwLock::new(HashMap::from([(id, make_test_project(id))])));
        let deliveries = crate::commands::human_review::DeliveriesInFlight::default();

        let _serial = super::delete_junction_test_support::serial().await;
        let (mut arrived, release_tx) = super::delete_junction_test_support::arm(id);
        let gate = crate::lifecycle::ProjectLifecycleGate::new();
        let locks = crate::lifecycle::TaskLifecycleLocks::new();
        let deletion = {
            let projects = projects.clone();
            let tasks = tasks.clone();
            let storage = storage.clone();
            let deliveries = deliveries.clone();
            async move {
                let _gate = gate.write(id).await;
                let _leases = crate::lifecycle::hold_project_task_leases(&locks, &tasks, id)
                    .await
                    .expect("no lifecycle operation owns these tasks in this test");
                delete_project_committed(&projects, &tasks, &storage, None, Some(&deliveries), id)
                    .await
            }
        };
        let del_task = tokio::spawn(deletion);
        arrived.recv().await.expect("a junction cannot close early");

        // The delivery begins after the refusal pass, while the deletion is
        // held: exactly the window a bare refusal pass would miss.
        let _mark = deliveries.start(task.id);

        let _ = release_tx.send(());
        let refused = del_task
            .await
            .expect("the deletion must not panic")
            .expect_err("the delivery marker must refuse the deletion");
        drop(_mark);
        assert!(
            refused.contains("a pull request is being opened"),
            "the refusal must name the delivery: {refused:?}"
        );

        // The refusal wrote nothing: no retry record, board, records and Task
        // file all survive.
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(id)
                .exists(),
            "no Task cleanup record is owed for a deletion that never began"
        );
        assert!(
            !storage
                .paths()
                .pending_project_conversation_deletion_file(id)
                .exists(),
            "no Conversation cleanup record is owed for a deletion that never began"
        );
        assert!(
            projects.read().await.contains_key(&id),
            "a refused deletion must not have removed the Project from memory"
        );
        assert!(
            storage
                .load_config_strict()
                .unwrap()
                .projects
                .contains_key(&id.to_string()),
            "a refused deletion must not have removed the Project from disk"
        );
        assert_eq!(tasks.read().await.len(), 1);
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
    }

    /// A `repository_id` update re-derives the routes. Held behind the shared
    /// gate, it cannot move the board files out from under a deletion that is
    /// capturing them: it waits for the transaction to end and finds no
    /// Project to update.
    #[tokio::test]
    async fn a_route_change_cannot_move_a_being_deleted_project_around_the_deletion() {
        let (storage, temp) = create_test_storage();
        let id = Uuid::new_v4();
        let repo_one = temp.path().join("repo-one");
        let repo_two = temp.path().join("repo-two");
        std::fs::create_dir_all(&repo_one).unwrap();
        std::fs::create_dir_all(&repo_two).unwrap();
        let repository_one = Uuid::new_v4();
        let repository_two = Uuid::new_v4();
        let repository = |local_path: std::path::PathBuf| crate::domain::Repository {
            id: Uuid::new_v4(),
            local_path: local_path.to_string_lossy().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        };
        let mut config = AppConfig::default();
        config
            .repositories
            .insert(repository_one.to_string(), repository(repo_one.clone()));
        config
            .repositories
            .insert(repository_two.to_string(), repository(repo_two.clone()));
        let mut project = make_test_project(id);
        project.repository_id = Some(repository_one);
        config.projects.insert(id.to_string(), project);
        storage.save_config(&config).unwrap();

        // The board lives at the route derived from repository one.
        let task = board_task(id);
        storage.save_project_tasks(id, std::slice::from_ref(&task)).unwrap();
        let routed = storage.tasks_board_files(id);
        assert!(
            routed.iter().any(|p| p.exists()),
            "the routed board file must exist before the deletion: {routed:?}"
        );

        let tasks = board(vec![task]);
        let projects = Arc::new(RwLock::new(HashMap::from([(id, make_test_project(id))])));

        let _serial = super::delete_junction_test_support::serial().await;
        let (mut arrived, release_tx) = super::delete_junction_test_support::arm(id);
        let gate = crate::lifecycle::ProjectLifecycleGate::new();
        let locks = crate::lifecycle::TaskLifecycleLocks::new();
        let deletion = {
            let projects = projects.clone();
            let tasks = tasks.clone();
            let storage = storage.clone();
            let gate = gate.clone();
            async move {
                let _gate = gate.write(id).await;
                let _leases = crate::lifecycle::hold_project_task_leases(&locks, &tasks, id)
                    .await
                    .expect("no lifecycle operation owns these tasks in this test");
                delete_project_committed(&projects, &tasks, &storage, None, None, id).await
            }
        };
        let del_task = tokio::spawn(deletion);
        arrived.recv().await.expect("a junction cannot close early");

        // The route-changing update queues on the shared gate while the
        // deletion holds it exclusively.
        let update = {
            let projects = projects.clone();
            let storage = storage.clone();
            let gate = gate.clone();
            async move {
                let _gate_read = gate.read(id).await;
                update_project_committed(
                    &projects,
                    &storage,
                    id,
                    None,
                    Some(repository_two.to_string()),
                    None,
                )
                .await
            }
        };
        let update_task = tokio::spawn(update);

        let _ = release_tx.send(());
        let (deleted, updated) = tokio::join!(del_task, update_task);
        let deleted = deleted.expect("the deletion must not panic");
        let updated = updated.expect("the update must not panic");

        assert!(deleted.expect("the deletion must succeed"), "the deletion committed");
        assert!(
            matches!(updated, Ok(None)),
            "the queued update must find the Project gone, not re-route it: {updated:?}"
        );
        for file in &routed {
            assert!(
                !file.exists(),
                "the committed deletion removed every captured board file: {file:?}"
            );
        }
        assert!(
            !storage
                .load_config_strict()
                .unwrap()
                .projects
                .contains_key(&id.to_string()),
            "the removal must be persisted"
        );
        assert!(tasks.read().await.is_empty());
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 0);
    }

    /// CodeRabbit: a failed Task-file cleanup against orphans whose Project
    /// the config already lost must still produce a retry record — written
    /// before anything is deleted, not after the failure.
    #[tokio::test]
    async fn an_orphan_tasks_file_gains_a_retry_record_before_its_cleanup() {
        let (storage, _temp) = create_test_storage();
        let id = Uuid::new_v4();
        storage.save_config(&AppConfig::default()).unwrap();

        // An orphan Task file for a Project that no longer exists: the legacy
        // candidate is what an unrouted Project reads.
        let task = board_task(id);
        let tasks = board(Vec::new());
        storage.save_project_tasks(id, std::slice::from_ref(&task)).unwrap();
        let orphan = storage.tasks_board_files(id);
        assert!(orphan.first().is_some_and(|path| path.exists()));
        let orphan_path = orphan
            .into_iter()
            .find(|path| path.exists())
            .expect("the orphan file exists");

        let _serial = super::delete_junction_test_support::serial().await;
        let (mut arrived, release_tx) = super::delete_junction_test_support::arm(id);
        let gate = crate::lifecycle::ProjectLifecycleGate::new();
        let locks = crate::lifecycle::TaskLifecycleLocks::new();
        let projects = Arc::new(RwLock::new(HashMap::new()));
        let deletion = {
            let projects = projects.clone();
            let tasks = tasks.clone();
            let storage = storage.clone();
            let gate = gate.clone();
            async move {
                let _gate = gate.write(id).await;
                let _leases = crate::lifecycle::hold_project_task_leases(&locks, &tasks, id)
                    .await
                    .expect("no lifecycle operation owns these tasks in this test");
                delete_project_committed(&projects, &tasks, &storage, None, None, id).await
            }
        };
        let del_task = tokio::spawn(deletion);
        arrived.recv().await.expect("a junction cannot close early");

        // Make the removal impossible in a root-proof way: a directory cannot
        // be unlinked with remove_file, whatever the user is.
        std::fs::remove_file(&orphan_path).unwrap();
        std::fs::create_dir(&orphan_path).unwrap();
        std::fs::write(orphan_path.join("inner.toml"), "tasks = []").unwrap();

        let _ = release_tx.send(());
        let refused = del_task
            .await
            .expect("the deletion must not panic")
            .expect_err("the orphan cleanup must be refused loudly");
        assert!(
            refused.contains("cleanup remains pending"),
            "the failure must say what the next start must finish: {refused:?}"
        );
        assert!(
            storage
                .paths()
                .pending_project_tasks_deletion_file(id)
                .exists(),
            "the retry record must exist despite the failed cleanup"
        );

        // Remove the blocker; a retried attempt enacts exactly the record it
        // wrote, and clears it.
        std::fs::remove_dir_all(&orphan_path).unwrap();
        let retried = delete_project_committed(&projects, &tasks, &storage, None, None, id)
            .await
            .expect("the retry must not panic");
        assert!(retried, "the retry still deletes something durable");
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(id)
                .exists(),
            "the successful cleanup clears its retry record"
        );
        assert!(tasks.read().await.is_empty());
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 0);
    }

    /// A first attempt's cleanup record may name board files the recompute
    /// can no longer see (the routed file a config-lost Project's route
    /// table no longer answers for). A retried attempt must keep that capture
    /// — a successful retry then disposes of it too.
    #[tokio::test]
    async fn a_retry_deletion_keeps_the_first_capture_in_its_retry_record() {
        let (storage, temp) = create_test_storage();
        let id = Uuid::new_v4();
        storage.save_config(&AppConfig::default()).unwrap();

        // The orphan an unrouted Project reads, plus the file an earlier
        // attempt captured while the Project was still routed.
        let task = board_task(id);
        let tasks = board(Vec::new());
        storage.save_project_tasks(id, std::slice::from_ref(&task)).unwrap();
        let legacy = storage
            .tasks_board_files(id)
            .into_iter()
            .find(|path| path.exists())
            .expect("the orphan file exists");
        let routed = temp.path().join("state").join("routed").join("tasks.toml");
        std::fs::create_dir_all(routed.parent().unwrap()).unwrap();
        std::fs::write(&routed, "tasks = []").unwrap();
        // As if a first attempt captured both before the config lost the
        // Project.
        storage
            .mark_project_tasks_deletion_pending(id, &[legacy.clone(), routed.clone()])
            .unwrap();

        let _serial = super::delete_junction_test_support::serial().await;
        let (mut arrived, release_tx) = super::delete_junction_test_support::arm(id);
        let gate = crate::lifecycle::ProjectLifecycleGate::new();
        let locks = crate::lifecycle::TaskLifecycleLocks::new();
        let projects = Arc::new(RwLock::new(HashMap::new()));
        let deletion = {
            let projects = projects.clone();
            let tasks = tasks.clone();
            let storage = storage.clone();
            let gate = gate.clone();
            async move {
                let _gate = gate.write(id).await;
                let _leases = crate::lifecycle::hold_project_task_leases(&locks, &tasks, id)
                    .await
                    .expect("no lifecycle operation owns these tasks in this test");
                delete_project_committed(&projects, &tasks, &storage, None, None, id).await
            }
        };
        let del_task = tokio::spawn(deletion);
        arrived.recv().await.expect("a junction cannot close early");

        // Make the removal impossible in a root-proof way.
        std::fs::remove_file(&legacy).unwrap();
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("inner.toml"), "tasks = []").unwrap();

        let _ = release_tx.send(());
        let refused = del_task
            .await
            .expect("the deletion must not panic")
            .expect_err("the orphan cleanup must be refused loudly");
        assert!(
            refused.contains("cleanup remains pending"),
            "the failure must say what the next start must finish: {refused:?}"
        );
        // The retry record kept the first attempt's capture, not only the
        // candidates a config without the Project recomputes.
        let record = storage
            .pending_project_tasks_deletions()
            .unwrap()
            .into_iter()
            .find(|record| record.project_id == id)
            .expect("the retry record exists");
        assert!(
            record.board_files.contains(&routed),
            "the first capture must survive a retried attempt's rewrite: {record:?}"
        );

        // Remove the blocker; the retry enacts exactly the union it recorded
        // and clears it.
        std::fs::remove_dir_all(&legacy).unwrap();
        let retried = delete_project_committed(&projects, &tasks, &storage, None, None, id)
            .await
            .expect("the retry must not panic");
        assert!(retried);
        assert!(!legacy.exists() && !routed.exists(), "everything the record named is gone");
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(id)
                .exists()
        );
    }

    /// A board rooted in a repository path carrying a line break cannot be
    /// named faithfully by the one-path-per-line cleanup record, so deletion
    /// refuses before anything is written instead of encoding it lossily.
    #[tokio::test]
    async fn a_board_file_path_with_a_line_break_cannot_be_deleted() {
        let (storage, temp) = create_test_storage();
        let id = Uuid::new_v4();
        // A repository root with a newline in it; the in-project state joins
        // `.slashit` to that root, so the whole board path carries it.
        let repo = temp.path().join("line\nbreak repo");
        std::fs::create_dir_all(repo.join(".slashit").join(id.to_string())).unwrap();
        let repository_id = Uuid::new_v4();
        let mut config = AppConfig::default();
        config.repositories.insert(
            repository_id.to_string(),
            crate::domain::Repository {
                id: repository_id,
                local_path: repo.to_string_lossy().to_string(),
                remote_url: None,
                remote_type: None,
                created_at: chrono::Utc::now(),
            },
        );
        let mut project = make_test_project(id);
        project.repository_id = Some(repository_id);
        project.state_location = crate::config::paths::StateLocation::InProject;
        config.projects.insert(id.to_string(), project);
        storage.save_config(&config).unwrap();

        let task = board_task(id);
        storage.save_project_tasks(id, std::slice::from_ref(&task)).unwrap();
        let board_files = storage.tasks_board_files(id);
        assert!(
            board_files.iter().any(|p| p.to_str().is_some_and(|t| t.contains('\n'))),
            "this fixture must produce a board path with a newline: {board_files:?}"
        );

        let tasks = board(vec![task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));

        let result = delete_project_committed(&projects, &tasks, &storage, None, None, id).await;
        let error = result.expect_err("the deletion must be refused");
        assert!(
            error.contains("cannot faithfully") || error.contains("not valid UTF-8") || error.contains("line break"),
            "the refusal must say why the paths are unrecordable: {error:?}"
        );

        // The refusal happened before anything was written.
        assert!(
            !storage
                .paths()
                .pending_project_tasks_deletion_file(id)
                .exists(),
            "no Task cleanup record is owed for a deletion that never began"
        );
        assert!(
            !storage
                .paths()
                .pending_project_conversation_deletion_file(id)
                .exists(),
            "no Conversation cleanup record is owed for a deletion that never began"
        );
        assert_eq!(tasks.read().await.len(), 1);
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
        assert!(
            projects.read().await.contains_key(&id),
            "the Project survives a refusal this early"
        );
    }

    #[tokio::test]
    async fn an_interrupted_cleanup_refuses_deletion() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let mut task = board_task(id);
        task.cleanup_in_flight = true;
        let tasks = seed_board(id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));

        refused_obligation(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );
        assert_eq!(storage.load_project_tasks(id).unwrap().len(), 1);
    }

    /// A cleared-up checkout and the Git recovery refs it left behind: a
    /// refused Project deletion touches none of them, and the eligible one
    /// does not either (only lifecycle operations ever do).
    #[cfg(unix)]
    #[test]
    fn git_recovery_refs_survive_both_refused_and_successful_deletion() {
        let root = tempfile::TempDir::new().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .expect("git must be spawnable");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        // A detached default branch is enough: recovery refs only need HEAD.
        run(&["init", "-q", "-b", "main"]);
        run(&[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=T",
            "commit",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let head = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let head = String::from_utf8(head.stdout).unwrap().trim().to_string();

        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        storage.save_config(&AppConfig::default()).unwrap();
        let project_id = project.id;

        let pinned = format!("refs/slashit/integration/{project_id}-task");
        let base = "refs/slashit/integrated-base/main".to_string();
        run(&["update-ref", &pinned, &head]);
        run(&["update-ref", &base, &head]);

        // Refused: a pending integration pins a ref.
        let mut task = board_task(project_id);
        task.worktree_path = Some("/gone/checkout".to_string());
        task.branch_name = Some("slashit/task".to_string());
        task.local_integration = Some(pending_integration());

        let tasks = seed_board(project_id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(project_id, make_test_project(project_id))]));
        refused(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(delete_project_committed(
                    &projects, &tasks, &storage, None, None, project_id,
                )),
            project_id,
            &storage,
        );

        for (expected_ref, expected_value) in [(&pinned, &head), (&base, &head)] {
            let value = std::process::Command::new("git")
                .args(["rev-parse", expected_ref])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&value.stdout).trim(),
                *expected_value
            );
        }

        // Then the deletion the check allows: a finishable integration.
        let ok_task = board_task(project_id);
        let tasks = seed_board(project_id, &storage, &[ok_task]);
        let projects = RwLock::new(HashMap::from([(project_id, make_test_project(project_id))]));
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(delete_project_committed(
                &projects, &tasks, &storage, None, None, project_id,
            ))
            .expect("an eligible Project deletes");

        for (expected_ref, expected_value) in [(&pinned, &head), (&base, &head)] {
            let value = std::process::Command::new("git")
                .args(["rev-parse", expected_ref])
                .current_dir(&repo)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&value.stdout).trim(),
                *expected_value
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_read_failed_task_records_block_deletion_before_anything_is_written() {
        // An unreadable board cannot be checked, so deletion is refused and
        // nothing — config, records, files — changes.
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage
            .save_config(&AppConfig {
                projects: HashMap::from([(id.to_string(), make_test_project(id))]),
                ..AppConfig::default()
            })
            .unwrap();
        let task = board_task(id);
        storage.save_project_tasks(id, &[task]).unwrap();
        let tasks = board(vec![]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        let tasks_file = board_tasks_file(id, &storage);
        std::fs::write(&tasks_file, b"damaged but unreadable[[[").unwrap();
        std::fs::set_permissions(&tasks_file, std::fs::Permissions::from_mode(0o000)).unwrap();

        refused(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );

        std::fs::set_permissions(&tasks_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(projects.read().await.contains_key(&id));
        assert!(
            std::fs::read(&tasks_file).unwrap() == b"damaged but unreadable[[[",
            "the damaged record was not overwritten"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_task_file_deletion_failure_is_reported_as_a_pending_cleanup() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let tasks_dir = storage.paths().config_dir().join("tasks");
        let tasks_file = board_tasks_file(id, &storage);
        let task = board_task(id);
        let tasks = seed_board(id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        // The directory holding the Task file denies deletion.
        std::fs::set_permissions(&tasks_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = delete_project_committed(&projects, &tasks, &storage, None, None, id).await;

        // Deletion is allowed, but it is not a success: the Task records
        // remain, cleanup is pending, and the record for a restart is there.
        let error = result.expect_err("a failed Task-file deletion must be reported");
        assert!(error.contains("pending"), "{error:?}");
        assert!(
            !projects.read().await.contains_key(&id),
            "the config commit stands"
        );
        assert!(
            tasks_file.exists(),
            "the Task record survives for the retry record"
        );
        assert!(storage
            .paths()
            .pending_project_tasks_deletion_file(id)
            .exists());
        assert_eq!(
            storage.load_project_tasks(id).unwrap().len(),
            1,
            "disk keeps the record"
        );
        assert!(
            tasks.read().await.values().any(|t| t.project_id == id),
            "the board keeps what the disk kept: memory must not publish less than disk has"
        );
        std::fs::set_permissions(&tasks_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        // The retry record makes the next start finish what was owed; after
        // the fix locally, a retry deletes the task records now that the
        // directory allows deletion again.
        assert!(storage.delete_task_files(&storage.tasks_board_files(id)).is_ok());
        assert!(storage.clear_project_tasks_deletion_pending(id).is_ok());
    }


    /// The routed case of issue #193, which only the captured board files in
    /// the retry record can close once the Project is out of the config: a
    /// project with a repository routes its board into its own state
    /// directory, and the deletion must reach exactly that file.
    #[tokio::test]
    async fn a_routed_projects_task_file_is_deleted_with_the_recorded_board_files() {
        let (storage, temp) = create_test_storage();
        let repo_dir = temp.path().join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let repository_id = Uuid::new_v4();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        let mut routed_project = project;
        routed_project.repository_id = Some(repository_id);
        storage.save_config(&AppConfig {
            repositories: HashMap::from([(
                repository_id.to_string(),
                crate::domain::Repository {
                    id: repository_id,
                    local_path: repo_dir.to_string_lossy().to_string(),
                    remote_url: None,
                    remote_type: None,
                    created_at: chrono::Utc::now(),
                },
            )]),
            projects: HashMap::from([(id.to_string(), routed_project.clone())]),
            ..AppConfig::default()
        }).unwrap();
        storage.load_config().unwrap();
        let board_files = storage.tasks_board_files(id);
        assert!(
            board_files.len() >= 2,
            "a routed project records both its routed board and the legacy one"
        );
        let routed_file = &board_files[0];
        assert!(routed_file != &board_files[1], "the two candidates must differ");

        // A pending integration pins the record, so the refusal still holds
        // for a routed project, and the routed file survives it.
        let mut task = board_task(id);
        task.status = crate::domain::TaskStatus::InProgress;
        task.local_integration = Some(pending_integration());
        storage.save_project_tasks(id, std::slice::from_ref(&task)).unwrap();
        let tasks = board(vec![task]);
        let projects = RwLock::new(HashMap::from([(id, routed_project.clone())]));
        refused_obligation(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );
        assert!(routed_file.exists());

        // Then the deletion the checks allow: the routed file is the one that
        // goes, by name, and no later save can bring a second copy back
        // through the legacy path.
        let done = board_task(id);
        storage.save_project_tasks(id, std::slice::from_ref(&done)).unwrap();
        let tasks = board(vec![done]);
        let projects = RwLock::new(HashMap::from([(id, routed_project.clone())]));
        assert!(
            delete_project_committed(&projects, &tasks, &storage, None, None, id)
                .await
                .unwrap()
        );
        for board_file in &board_files {
            assert!(!board_file.exists(), "{board_file:?} must be gone");
        }
        assert!(storage.load_project_tasks(id).unwrap().is_empty());
        // And a restart-side recreation of the marker records nothing new.
        assert!(!storage.paths().pending_project_tasks_deletion_file(id).exists());
    }

    #[tokio::test]
    async fn task_creation_after_project_deletion_is_refused_and_leaves_no_file() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let tasks = board(vec![board_task(id)]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        let gate = crate::lifecycle::ProjectLifecycleGate::new();
        assert!(
            delete_project_committed(&projects, &tasks, &storage, None, None, id)
                .await
                .unwrap()
        );

        let recreated = crate::lifecycle::create(&gate, &tasks, &projects, &storage, id, |existing| {
            let tags = crate::domain::NewTask {
                id: Uuid::new_v4(),
                project_id: id,
                title: "Too late".to_string(),
                description: None,
                model: "test".to_string(),
                planning_mode: false,
                dependencies: Vec::new(),
                category: Default::default(),
                priority: Default::default(),
                complexity: Default::default(),
                impact: Default::default(),
                security_severity: Default::default(),
                github_issue_url: None,
                gitlab_issue_url: None,
                linear_ticket_id: None,
            };
            crate::domain::Task::new_backlog(existing, tags)
        })
        .await;
        assert!(
            recreated.is_err(),
            "a Task must not be created for a deleted Project"
        );
        assert_eq!(
            storage.load_project_tasks(id).unwrap().len(),
            0,
            "the deleted Project's Task file was not recreated"
        );
    }

    /// A Tauri-flavored creation for same project id must be blocked while
    /// the deletion commits. Since deletion holds the shared board's write
    /// guard from the project removal to memory cleanup, awaiting a create
    /// inside that window cannot interleave.
    #[tokio::test]
    async fn a_concurrent_task_creation_cannot_interleave_with_deletion() {
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage.save_config(&AppConfig::default()).unwrap();
        let tasks = board(vec![board_task(id)]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        let gate = crate::lifecycle::ProjectLifecycleGate::new();

        // Simulate a deletion in flight: hold the board's write guard across
        // the same interval delete_project_committed does.
        let guard = tasks.write().await;

        let creation = crate::lifecycle::create(&gate, &tasks, &projects, &storage, id, |_| {
            let mut t = crate::test_helpers::create_test_task("Racing");
            t.project_id = id;
            t
        });
        let raced = tokio::time::timeout(std::time::Duration::from_millis(100), creation).await;
        assert!(
            raced.is_err(),
            "the create must wait for the deletion's write guard, not interleave"
        );
        drop(guard);

        // The deletion then commits for real. The Task here is the same one
        // the interrupted create raced against; with the guard released the
        // deletion's write guard is free and the create cannot land inside
        // it anymore — it simply decides afterwards and is refused.
        assert!(
            delete_project_committed(&projects, &tasks, &storage, None, None, id)
                .await
                .unwrap(),
            "the deletion itself must have succeeded"
        );
        let result = crate::lifecycle::create(&gate, &tasks, &projects, &storage, id, |_| {
            let mut t = crate::test_helpers::create_test_task("Racing");
            t.project_id = id;
            t
        })
        .await;
        assert!(
            result.is_err(),
            "creation for a deleted Project must be refused"
        );
        assert!(
            !storage
                .paths()
                .config_dir()
                .join("tasks")
                .join(format!("{id}.toml"))
                .exists(),
            "no Task file may reappear for a deleted Project"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_deletion_survives_a_restart_untouched() {
        if crate::ipc::server::current_uid() == 0 {
            return;
        }
        let (storage, _temp) = create_test_storage();
        let project = make_test_project(Uuid::new_v4());
        let id = project.id;
        storage
            .save_config(&AppConfig {
                projects: HashMap::from([(id.to_string(), make_test_project(id))]),
                ..AppConfig::default()
            })
            .unwrap();
        let mut task = board_task(id);
        task.local_integration = Some(pending_integration());
        let tasks = seed_board(id, &storage, &[task]);
        let projects = RwLock::new(HashMap::from([(id, make_test_project(id))]));
        refused(
            delete_project_committed(&projects, &tasks, &storage, None, None, id).await,
            id,
            &storage,
        );

        // A restart hydrates from the same disk; nothing changed, so
        // everything comes back and no cleanup record asks for anything.
        assert_eq!(
            storage.clone().load_project_tasks(id).unwrap().len(),
            1,
            "the Task record is exactly where it was"
        );
        assert!(storage
            .clone()
            .load_config_strict()
            .unwrap()
            .projects
            .contains_key(&id.to_string()));
        assert!(!storage
            .paths()
            .pending_project_tasks_deletion_file(id)
            .exists());
        assert!(!storage
            .paths()
            .pending_project_conversation_deletion_file(id)
            .exists());
    }
}
