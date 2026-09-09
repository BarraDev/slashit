//! Commands for inspecting and changing where a project's state is stored.
//!
//! The user-facing contract is narrow on purpose: a project's *shareable*
//! state (its board) can live outside the project or inside it, and moving
//! between the two is an explicit, previewable action. Secrets, runtime files
//! and machine-local caches are not part of that choice and are never offered
//! — see [`crate::config::paths`] for the classification.

use crate::config::migration::{ConflictPolicy, MigrationPlan, MigrationReport, StateMigrator};
use crate::config::paths::{AppPaths, ProjectKey, ResolvedLocation, StateLocation};
use crate::config::Storage;
use crate::domain::{Project, Repository};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// Serializes `apply_state_migration` and `set_state_location` per project.
///
/// Keyed by project id, not by migration destination — the destination-keyed
/// filesystem lock inside [`StateMigrator`] only ever protects one direction
/// of one move; it does nothing to stop `set_state_location` (which touches
/// no files) from racing a concurrent migration for the same project. Every
/// entry point in this module that reads-then-writes a project's state
/// location acquires this guard first, so the two commands can never
/// interleave their read-decide-write sequence for one project.
///
/// Entries are never evicted: one idle `Arc<Mutex<()>>` per project that has
/// ever changed its state location is a handful of bytes, bounded by the
/// number of projects that exist, and not worth cleanup complexity.
#[derive(Default)]
pub struct StateLocationLocks {
    per_project: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
}

impl StateLocationLocks {
    pub fn new() -> Self {
        Self::default()
    }

    async fn acquire(&self, project_id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let handle = {
            let mut per_project = self.per_project.lock().await;
            per_project
                .entry(project_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        handle.lock_owned().await
    }
}

/// Everything the settings UI needs to describe a project's storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateLocationInfo {
    pub project_id: String,
    /// The stored preference, including `Auto`.
    pub location: StateLocation,
    /// What `Auto` currently resolves to. Shown so the user is never guessing.
    pub resolved: ResolvedLocation,
    pub project_root: String,
    /// Where state is right now.
    pub current_dir: String,
    pub external_dir: String,
    pub in_project_dir: String,
    /// False when the project has no repository, so there is no path to key
    /// storage off and the choice cannot be offered.
    pub can_choose: bool,
    /// RFC3339 timestamp of the project record this info was read from.
    ///
    /// Callers that later ask to change the location (in particular
    /// `set_state_location`, which trusts its caller rather than inspecting
    /// disk) pass this back as `known_updated_at` so a decision based on a
    /// stale read can be rejected instead of silently overwriting a newer one.
    pub updated_at: String,
}

/// Resolve the project and its repository root, or explain why we cannot.
///
/// Takes raw locks rather than `tauri::State` so it is reusable from the
/// `*_committed` helpers below, which tests drive without a full `AppState`.
async fn resolve_project_root(
    projects: &RwLock<HashMap<Uuid, Project>>,
    repositories: &RwLock<HashMap<Uuid, Repository>>,
    project_id: Uuid,
) -> Result<(Project, PathBuf), String> {
    let project = {
        let projects = projects.read().await;
        projects
            .get(&project_id)
            .cloned()
            .ok_or_else(|| format!("Project {project_id} not found"))?
    };

    let repository_id = project
        .repository_id
        .ok_or_else(|| "This project has no repository, so it has no folder to store state in.".to_string())?;

    let root = {
        let repositories = repositories.read().await;
        repositories
            .get(&repository_id)
            .map(|r| PathBuf::from(&r.local_path))
            .ok_or_else(|| format!("Repository {repository_id} not found"))?
    };

    Ok((project, root))
}

async fn project_root(
    state: &tauri::State<'_, crate::AppState>,
    project_id: Uuid,
) -> Result<(Project, PathBuf), String> {
    resolve_project_root(
        &state.project.projects,
        &state.repository.repositories,
        project_id,
    )
    .await
}

fn info_for(
    state: &tauri::State<'_, crate::AppState>,
    project: &Project,
    root: &Path,
) -> StateLocationInfo {
    let key = ProjectKey::for_path(root).key;
    let paths = &state.paths;

    StateLocationInfo {
        project_id: project.id.to_string(),
        location: project.state_location,
        resolved: project.state_location.resolve(root),
        project_root: root.display().to_string(),
        current_dir: paths
            .project_state_dir(&key, project.id, root, project.state_location)
            .display()
            .to_string(),
        external_dir: paths
            .project_state_dir(&key, project.id, root, StateLocation::External)
            .display()
            .to_string(),
        in_project_dir: paths
            .project_state_dir(&key, project.id, root, StateLocation::InProject)
            .display()
            .to_string(),
        can_choose: true,
        updated_at: project.updated_at.to_rfc3339(),
    }
}

#[tauri::command]
pub async fn get_state_location(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
) -> Result<StateLocationInfo, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;

    match project_root(&state, id).await {
        Ok((project, root)) => Ok(info_for(&state, &project, &root)),
        Err(_) => {
            // A project with no repository still renders a settings panel; it
            // just cannot be given a choice. Returning an error instead would
            // make the whole panel look broken.
            let projects = state.project.projects.read().await;
            let project = projects
                .get(&id)
                .ok_or_else(|| format!("Project {id} not found"))?;
            Ok(StateLocationInfo {
                project_id: project.id.to_string(),
                location: project.state_location,
                resolved: ResolvedLocation::External,
                project_root: String::new(),
                current_dir: String::new(),
                external_dir: String::new(),
                in_project_dir: String::new(),
                can_choose: false,
                updated_at: project.updated_at.to_rfc3339(),
            })
        }
    }
}

/// Describe what moving to `target` would do, without touching anything.
#[tauri::command]
pub async fn plan_state_migration(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    target: StateLocation,
) -> Result<MigrationPlan, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let (project, root) = project_root(&state, id).await?;

    let key = ProjectKey::for_path(&root).key;
    let from = state
        .paths
        .project_state_dir(&key, project.id, &root, project.state_location);
    let to = state
        .paths
        .project_state_dir(&key, project.id, &root, target);

    StateMigrator::plan(&from, &to).map_err(|e| e.to_string())
}

/// Record `target` as project `id`'s new state location, or explain why not.
///
/// Split out from [`apply_state_migration`] so the race it guards against —
/// the project disappearing (e.g. deleted from another window) between the
/// migration finishing on disk and this point — is unit-testable without a
/// full `AppState`. The files have already moved by the time this runs, so
/// silently doing nothing for a vanished project would let the caller believe
/// the move fully succeeded when nothing was left to record it on.
fn record_migrated_location(
    projects: &mut HashMap<Uuid, Project>,
    id: Uuid,
    target: StateLocation,
    to: &Path,
) -> Result<(), String> {
    match projects.get_mut(&id) {
        Some(project) => {
            project.state_location = target;
            project.updated_at = chrono::Utc::now();
            Ok(())
        }
        None => Err(format!(
            "State was moved to {} but project {id} no longer exists, so the new \
             location could not be recorded on it.",
            to.display()
        )),
    }
}

/// Bundles the resource handles [`apply_state_migration_committed`] and
/// [`set_state_location_committed`] need, so passing them through stays under
/// clippy's argument-count lint without smuggling anything through global
/// state — the same shape [`crate::lifecycle::TerminalizeCtx`] uses.
struct StateLocationCtx<'a> {
    locks: &'a StateLocationLocks,
    projects: &'a RwLock<HashMap<Uuid, Project>>,
    repositories: &'a RwLock<HashMap<Uuid, Repository>>,
    paths: &'a AppPaths,
    storage: &'a Storage,
}

/// Core of [`apply_state_migration`], taking raw locks/paths/storage so tests
/// can drive it without a `tauri::State`.
///
/// The per-project guard is acquired before anything else, then the project's
/// current location is re-read under it — so a transition already recorded by
/// a concurrent call (`apply_state_migration` or `set_state_location`, for
/// the same project) is never migrated from a source directory that is no
/// longer current. The proposed map is persisted before it replaces the
/// shared one, so a persistence failure never leaves memory believing a
/// setting was saved that disk does not have.
async fn apply_state_migration_committed(
    ctx: StateLocationCtx<'_>,
    id: Uuid,
    target: StateLocation,
    policy: Option<ConflictPolicy>,
) -> Result<MigrationReport, String> {
    let _guard = ctx.locks.acquire(id).await;

    let (project, root) = resolve_project_root(ctx.projects, ctx.repositories, id).await?;

    let key = ProjectKey::for_path(&root).key;
    let from = ctx.paths.project_state_dir(&key, project.id, &root, project.state_location);
    let to = ctx.paths.project_state_dir(&key, project.id, &root, target);

    let report = StateMigrator::migrate(&from, &to, policy.unwrap_or_default())
        .map_err(|e| e.to_string())?;

    {
        let mut projects_w = ctx.projects.write().await;
        let mut proposed = projects_w.clone();
        record_migrated_location(&mut proposed, id, target, &to)?;

        // The files have already moved, so a failure to record where they
        // went must be reported rather than logged. Swallowing it would leave
        // the project reading from the directory the data just left, and the
        // user would open an empty board with nothing to explain it. Memory
        // is left holding the pre-migration map — consistent with what disk
        // still has — rather than a location the persisted config disagrees
        // with.
        crate::commands::project::try_persist_projects(ctx.storage, &proposed).map_err(|e| {
            format!(
                "State was moved to {} but the setting could not be saved: {e}. \
                 The move is safely on disk; re-select the location in Settings to finish.",
                to.display()
            )
        })?;

        *projects_w = proposed;
    }

    // Saving config rebuilt the routing table, so subsequent task reads and
    // writes already resolve to the new directory.
    Ok(report)
}

/// Move the project's state and persist the new preference.
///
/// The preference is written only after the move succeeds. Recording it first
/// would leave the project pointing at a directory the data never reached.
#[tauri::command]
pub async fn apply_state_migration(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    target: StateLocation,
    policy: Option<ConflictPolicy>,
) -> Result<MigrationReport, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    apply_state_migration_committed(
        StateLocationCtx {
            locks: &state.state_location_locks,
            projects: &state.project.projects,
            repositories: &state.repository.repositories,
            paths: &state.paths,
            storage: &state.storage,
        },
        id,
        target,
        policy,
    )
    .await
}

/// Parse the caller-supplied `known_updated_at` into the timestamp
/// [`set_state_location_committed`] compares against the authoritative
/// record.
///
/// A pure, side-effect-free step deliberately kept separate from
/// [`set_state_location`] so an empty or malformed value is rejected before
/// the per-project lock is even acquired, let alone before anything is
/// persisted.
fn parse_known_updated_at(raw: &str) -> Result<chrono::DateTime<chrono::Utc>, String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            format!("A known_updated_at timestamp is required to change the storage location: {e}")
        })
}

/// Core of [`set_state_location`], taking raw locks/storage so tests can
/// drive it without a `tauri::State`.
///
/// Shares [`StateLocationLocks`] with [`apply_state_migration_committed`] so
/// the two can never interleave for one project, and rejects a request whose
/// `known_updated_at` no longer matches the authoritative record — the
/// caller observed the project before a newer decision (a migration or
/// another `set_state_location`) was recorded, and applying it now would
/// silently un-record that newer decision. Unlike an earlier version of this
/// function, the check is not optional: there is no argument shape that
/// skips it, so a caller cannot accidentally (or otherwise) bypass staleness
/// protection by omitting the timestamp.
async fn set_state_location_committed(
    locks: &StateLocationLocks,
    projects: &RwLock<HashMap<Uuid, Project>>,
    storage: &Storage,
    id: Uuid,
    location: StateLocation,
    known_updated_at: chrono::DateTime<chrono::Utc>,
) -> Result<(), String> {
    let _guard = locks.acquire(id).await;

    let mut projects_w = projects.write().await;
    let mut proposed = projects_w.clone();
    let project = proposed
        .get_mut(&id)
        .ok_or_else(|| format!("Project {id} not found"))?;

    if project.updated_at != known_updated_at {
        return Err(
            "This project's storage location changed since it was last loaded here. \
             Reload the storage settings and try again."
                .to_string(),
        );
    }

    project.state_location = location;
    project.updated_at = chrono::Utc::now();

    crate::commands::project::try_persist_projects(storage, &proposed)
        .map_err(|e| format!("Failed to save the storage location: {e}"))?;

    *projects_w = proposed;
    Ok(())
}

/// Set the preference without moving anything.
///
/// Offered separately because a user who has already moved their files by hand
/// needs a way to tell SlashIt where they are, and forcing a migration through
/// would then be wrong.
///
/// `known_updated_at` is required and must be the `updated_at` from the last
/// [`StateLocationInfo`] the caller read (RFC3339) — this command shares its
/// per-project lock with [`apply_state_migration`], and a caller that skipped
/// the staleness check could otherwise silently undo a migration that
/// completed after it last read the project's state.
#[tauri::command]
pub async fn set_state_location(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    location: StateLocation,
    known_updated_at: String,
) -> Result<StateLocationInfo, String> {
    let id = Uuid::parse_str(&project_id).map_err(|e| e.to_string())?;
    let known_updated_at = parse_known_updated_at(&known_updated_at)?;

    set_state_location_committed(
        &state.state_location_locks,
        &state.project.projects,
        &state.storage,
        id,
        location,
        known_updated_at,
    )
    .await?;

    get_state_location(state, project_id).await
}

/// Remove the empty `.slashit/` directories that earlier versions created.
///
/// Only ever removes a directory it can prove is empty, so a populated
/// in-project state directory is left alone even if the project has since been
/// switched to external storage.
#[tauri::command]
pub async fn clean_legacy_state_dirs(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<String>, String> {
    let roots: Vec<PathBuf> = {
        let repositories = state.repository.repositories.read().await;
        repositories
            .values()
            .map(|r| PathBuf::from(&r.local_path))
            .collect()
    };

    let mut removed = Vec::new();
    for root in roots {
        let dir = crate::config::paths::AppPaths::in_project_state(&root);
        if StateMigrator::remove_empty_legacy_dir(&dir) {
            removed.push(dir.display().to_string());
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentConfig, AgentType, ProjectScope};

    fn test_project(id: Uuid) -> Project {
        Project {
            id,
            name: "Test Project".to_string(),
            repository_id: None,
            scope: ProjectScope::Standalone,
            state_location: StateLocation::External,
            agent_type: AgentType::ClaudeCode,
            agent_config: AgentConfig {
                agent_type: AgentType::ClaudeCode,
                command: "claude".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                model: None,
                api_key: None,
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn records_the_new_location_when_the_project_still_exists() {
        let id = Uuid::new_v4();
        let mut projects = HashMap::new();
        projects.insert(id, test_project(id));

        record_migrated_location(&mut projects, id, StateLocation::InProject, Path::new("/tmp/to"))
            .expect("project exists, this must succeed");

        assert_eq!(projects[&id].state_location, StateLocation::InProject);
    }

    #[test]
    fn errors_instead_of_silently_succeeding_when_the_project_vanished() {
        // Reproduces the race: the filesystem migration already completed
        // (the caller always calls this only after `StateMigrator::migrate`
        // returns `Ok`), but the project record is gone by the time the
        // result needs to be recorded — e.g. deleted from another window
        // while the migration was in flight.
        let id = Uuid::new_v4();
        let mut projects: HashMap<Uuid, Project> = HashMap::new();

        let result =
            record_migrated_location(&mut projects, id, StateLocation::InProject, Path::new("/tmp/to"));

        assert!(
            result.is_err(),
            "a vanished project must never be reported as a successful move"
        );
        assert!(result.unwrap_err().contains("no longer exists"));
    }

    #[tokio::test]
    async fn state_location_locks_serialize_the_same_project() {
        let locks = StateLocationLocks::new();
        let id = Uuid::new_v4();

        let guard = locks.acquire(id).await;

        let inner = {
            let per_project = locks.per_project.lock().await;
            per_project.get(&id).unwrap().clone()
        };
        assert!(
            inner.try_lock().is_err(),
            "a held per-project guard must block a concurrent holder for the same project"
        );

        drop(guard);
        assert!(
            inner.try_lock().is_ok(),
            "the project's guard must release once the holder drops it"
        );
    }

    #[tokio::test]
    async fn state_location_locks_do_not_serialize_different_projects() {
        let locks = StateLocationLocks::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let _guard_a = locks.acquire(a).await;

        let guard_b = tokio::time::timeout(std::time::Duration::from_millis(500), locks.acquire(b)).await;
        assert!(
            guard_b.is_ok(),
            "a different project must be acquirable without waiting on an unrelated project's guard"
        );
    }

    fn test_repository(id: Uuid, local_path: &Path) -> Repository {
        Repository {
            id,
            local_path: local_path.display().to_string(),
            remote_url: None,
            remote_type: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn create_test_storage() -> (Storage, tempfile::TempDir) {
        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
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

    /// Reproduces the exact race from the module docs: request A completes a
    /// real migration and records the new location; request B, built from an
    /// observation taken before A ran, must not be able to overwrite it.
    #[tokio::test]
    async fn a_stale_set_state_location_cannot_undo_a_completed_migration() {
        let (storage, _storage_temp) = create_test_storage();
        let repo_temp = tempfile::TempDir::new().expect("repo temp dir");
        let repo_root = repo_temp.path().to_path_buf();

        let project_id = Uuid::new_v4();
        let repository_id = Uuid::new_v4();

        let mut project = test_project(project_id);
        project.repository_id = Some(repository_id);
        let original_updated_at = project.updated_at;

        let projects = RwLock::new(HashMap::from([(project_id, project)]));
        let repositories = RwLock::new(HashMap::from([(
            repository_id,
            test_repository(repository_id, &repo_root),
        )]));
        let locks = StateLocationLocks::new();
        let paths = storage.paths();

        // Seed the external state dir with a file, so the migration actually
        // has data to move.
        let key = ProjectKey::for_path(&repo_root).key;
        let external_dir =
            paths.project_state_dir(&key, project_id, &repo_root, StateLocation::External);
        std::fs::create_dir_all(&external_dir).unwrap();
        std::fs::write(external_dir.join("board.json"), b"{}").unwrap();

        // A: a real migration to InProject.
        apply_state_migration_committed(
            StateLocationCtx {
                locks: &locks,
                projects: &projects,
                repositories: &repositories,
                paths,
                storage: &storage,
            },
            project_id,
            StateLocation::InProject,
            None,
        )
        .await
        .expect("migration should succeed");

        assert_eq!(
            projects.read().await.get(&project_id).unwrap().state_location,
            StateLocation::InProject,
            "the migration must be recorded"
        );

        // B: a stale `set_state_location`, built from the project's state
        // *before* A ran.
        let result = set_state_location_committed(
            &locks,
            &projects,
            &storage,
            project_id,
            StateLocation::External,
            original_updated_at,
        )
        .await;

        assert!(
            result.is_err(),
            "a request based on a stale observation must not overwrite a newer decision"
        );
        assert_eq!(
            projects.read().await.get(&project_id).unwrap().state_location,
            StateLocation::InProject,
            "the winning transition's location must survive a stale overwrite attempt"
        );

        // Persistence must be untouched too, not just memory: reload the
        // config from disk independently of the in-memory map and confirm
        // it still reflects the migration, not the stale rejected write.
        let persisted = storage.load_config().expect("config must still load");
        let persisted_project = persisted
            .projects
            .get(&project_id.to_string())
            .expect("project must still be present on disk");
        assert_eq!(
            persisted_project.state_location,
            StateLocation::InProject,
            "a rejected stale write must not reach disk"
        );
    }

    #[tokio::test]
    async fn set_state_location_committed_succeeds_when_known_updated_at_matches() {
        let (storage, _temp) = create_test_storage();
        let project_id = Uuid::new_v4();
        let project = test_project(project_id);
        let current = project.updated_at;
        let projects = RwLock::new(HashMap::from([(project_id, project)]));
        let locks = StateLocationLocks::new();

        set_state_location_committed(
            &locks,
            &projects,
            &storage,
            project_id,
            StateLocation::InProject,
            current,
        )
        .await
        .expect("a known_updated_at matching the authoritative record must be accepted");

        assert_eq!(
            projects.read().await.get(&project_id).unwrap().state_location,
            StateLocation::InProject
        );
    }

    #[test]
    fn parse_known_updated_at_rejects_an_empty_or_malformed_value() {
        assert!(
            parse_known_updated_at("").is_err(),
            "an empty timestamp must be rejected rather than silently skipping the staleness check"
        );
        assert!(parse_known_updated_at("not-a-timestamp").is_err());
    }

    #[test]
    fn parse_known_updated_at_accepts_a_valid_rfc3339_timestamp() {
        let now = chrono::Utc::now();
        let parsed =
            parse_known_updated_at(&now.to_rfc3339()).expect("a valid RFC3339 value must parse");
        // RFC3339 formatting truncates to the same precision it round-trips.
        assert_eq!(parsed.to_rfc3339(), now.to_rfc3339());
    }
}
