// SPDX-License-Identifier: MPL-2.0
use crate::commands::gui_admission::GuiMutation;
use crate::config::paths::AppPaths;
use crate::config::WorkspaceRegistry;
use crate::domain::{Project, Workspace, WorkspaceRoot};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

#[derive(Clone)]
pub struct WorkspaceState {
    pub registry: Arc<RwLock<WorkspaceRegistry>>,
}

impl WorkspaceState {
    /// Load the registry from `paths`, so a state built for a tempdir reads
    /// that tempdir's registry rather than the user's real one.
    pub fn load(paths: &AppPaths) -> Result<Self, std::io::Error> {
        let registry = WorkspaceRegistry::load_from(paths.workspaces_file())?;
        Ok(Self {
            registry: Arc::new(RwLock::new(registry)),
        })
    }
}

/// Where a Workspace's own context lives.
///
/// A Workspace with no custom root uses its SlashIt-managed root, which is
/// created with the Workspace. A custom root is an advanced choice of the
/// person and is never created, moved or deleted by SlashIt.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkspaceRootInfo {
    pub workspace_id: Uuid,
    pub path: String,
    pub custom: bool,
    pub exists: bool,
}

/// Validates an optional custom root. Empty input means no custom root.
fn custom_root(root_path: Option<String>) -> Result<Option<WorkspaceRoot>, String> {
    // Only blank input means no root: a trailing space can be part of a folder name.
    match root_path.filter(|p| !p.trim().is_empty()) {
        Some(path) => Ok(Some(WorkspaceRoot::try_new(&path)?)),
        None => Ok(None),
    }
}

/// Creates the managed root folder if it is missing. An existing folder is
/// left as it is.
fn ensure_managed_root(paths: &AppPaths, workspace_id: Uuid) -> Result<std::path::PathBuf, String> {
    let root = paths.workspace_managed_root(workspace_id);
    std::fs::create_dir_all(&root).map_err(|e| {
        format!("Cannot create the Workspace's managed folder {}: {e}", root.display())
    })?;
    Ok(root)
}

/// The folder a Workspace reads: its custom root, or its managed folder when it
/// has none. Canonical, so a symlinked data directory names the same folder
/// whether or not the managed folder exists yet.
fn effective_root(paths: &AppPaths, workspace: &Workspace) -> std::path::PathBuf {
    let raw = match &workspace.root_path {
        Some(custom) => custom.as_path().to_path_buf(),
        None => paths.workspace_managed_root(workspace.id),
    };
    canonical_with_missing_tail(&raw)
}

/// Canonicalizes the longest existing ancestor of `path` and re-appends the
/// names that do not exist yet. A path that cannot be resolved this way is
/// returned unchanged.
fn canonical_with_missing_tail(path: &std::path::Path) -> std::path::PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            return missing.iter().rev().fold(real, |acc, name| acc.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// The Workspace that already reads `root` (a canonical folder). `except` is
/// the Workspace being changed.
fn root_owner<'a>(
    registry: &'a WorkspaceRegistry,
    paths: &AppPaths,
    root: &std::path::Path,
    except: Option<Uuid>,
) -> Option<&'a Workspace> {
    registry
        .all()
        .find(|w| Some(w.id) != except && effective_root(paths, w) == root)
}

/// A Workspace that was not created. `workspace` is set only when the registry
/// already holds it (its save was published but not confirmed durable), so the
/// caller shows that Workspace instead of creating a second one.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CreateWorkspaceFailure {
    pub message: String,
    pub workspace: Option<Workspace>,
}

/// Creates a Workspace. Without a custom root it gets its managed root at once,
/// so the Workspace has an execution context from the start.
pub(crate) async fn create_workspace_core(
    registry: &RwLock<WorkspaceRegistry>,
    paths: &AppPaths,
    name: String,
    root_path: Option<String>,
) -> Result<Workspace, CreateWorkspaceFailure> {
    let refused = |message: String| CreateWorkspaceFailure {
        message,
        workspace: None,
    };
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(refused("Workspace name is required".to_string()));
    }
    let root = custom_root(root_path).map_err(refused)?;
    let mut registry = registry.write().await;
    if let Some(root) = &root {
        if root_owner(&registry, paths, root.as_path(), None).is_some() {
            return Err(refused(format!(
                "A workspace is already registered for {}",
                root.as_path().display()
            )));
        }
    }
    let workspace = Workspace::new(name, root);
    if workspace.root_path.is_none() {
        ensure_managed_root(paths, workspace.id).map_err(refused)?;
    }
    match registry.upsert(workspace.clone()) {
        Ok(()) => Ok(workspace),
        Err(error) => Err(CreateWorkspaceFailure {
            message: error.describe("save the workspace"),
            workspace: error.is_published().then_some(workspace),
        }),
    }
}

#[tauri::command]
pub async fn create_workspace(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    name: String,
    root_path: Option<String>,
) -> Result<Workspace, CreateWorkspaceFailure> {
    create_workspace_core(&state.workspace.registry, &state.paths, name, root_path).await
}

/// Changes a Workspace's custom root, or returns it to the managed root when
/// `root_path` is empty. Existing folders are never moved or deleted: the old
/// managed folder stays where it is.
pub(crate) async fn set_workspace_root_core(
    registry: &RwLock<WorkspaceRegistry>,
    paths: &AppPaths,
    workspace_id: Uuid,
    root_path: Option<String>,
) -> Result<Workspace, String> {
    let root = custom_root(root_path)?;
    let mut registry = registry.write().await;
    let mut workspace = registry
        .get(&workspace_id)
        .cloned()
        .ok_or_else(|| "Workspace not found".to_string())?;
    // Check the folder this change would leave in effect, whether it is the
    // custom root or the managed root that clearing returns to, before any write.
    let mut resulting = workspace.clone();
    resulting.root_path = root.clone();
    let resolved = effective_root(paths, &resulting);
    if let Some(owner) = root_owner(&registry, paths, &resolved, Some(workspace_id)) {
        return Err(format!(
            "The folder {} already belongs to the Workspace \"{}\"",
            resolved.display(),
            owner.name
        ));
    }
    if root.is_none() {
        ensure_managed_root(paths, workspace_id)?;
    }
    workspace.root_path = root;
    workspace.updated_at = chrono::Utc::now();
    registry.upsert(workspace.clone()).map_err(|e| e.describe("save the workspace root"))?;
    Ok(workspace)
}

#[tauri::command]
pub async fn set_workspace_root(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
    root_path: Option<String>,
) -> Result<Workspace, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    set_workspace_root_core(&state.workspace.registry, &state.paths, workspace_id, root_path).await
}

/// Renames a Workspace. Membership and the root are unchanged.
pub(crate) async fn rename_workspace_core(
    registry: &RwLock<WorkspaceRegistry>,
    workspace_id: Uuid,
    name: String,
) -> Result<Workspace, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Workspace name is required".to_string());
    }
    let mut registry = registry.write().await;
    let mut workspace = registry
        .get(&workspace_id)
        .cloned()
        .ok_or_else(|| "Workspace not found".to_string())?;
    workspace.name = name;
    workspace.updated_at = chrono::Utc::now();
    registry.upsert(workspace.clone()).map_err(|e| e.describe("rename the workspace"))?;
    Ok(workspace)
}

#[tauri::command]
pub async fn rename_workspace(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
    name: String,
) -> Result<Workspace, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    rename_workspace_core(&state.workspace.registry, workspace_id, name).await
}

/// The folder a Workspace's context lives in, as the registry and disk show
/// it now. A read: a missing managed folder is reported, not created.
pub(crate) async fn workspace_root_core(
    registry: &RwLock<WorkspaceRegistry>,
    paths: &AppPaths,
    workspace_id: Uuid,
) -> Result<WorkspaceRootInfo, String> {
    let workspace = registry
        .read()
        .await
        .get(&workspace_id)
        .cloned()
        .ok_or_else(|| "Workspace not found".to_string())?;
    match workspace.root_path {
        Some(root) => Ok(WorkspaceRootInfo {
            workspace_id,
            path: root.as_path().display().to_string(),
            custom: true,
            exists: root.as_path().is_dir(),
        }),
        None => {
            let root = paths.workspace_managed_root(workspace_id);
            Ok(WorkspaceRootInfo {
                workspace_id,
                path: root.display().to_string(),
                custom: false,
                exists: root.is_dir(),
            })
        }
    }
}

#[tauri::command]
pub async fn get_workspace_root(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<WorkspaceRootInfo, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    workspace_root_core(&state.workspace.registry, &state.paths, workspace_id).await
}

#[tauri::command]
pub async fn list_workspaces(
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<Workspace>, String> {
    let registry = state.workspace.registry.read().await;
    Ok(registry.all().cloned().collect())
}

#[tauri::command]
pub async fn get_workspace(
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<Workspace, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;
    let registry = state.workspace.registry.read().await;
    registry
        .get(&workspace_id)
        .cloned()
        .ok_or_else(|| "Workspace not found".to_string())
}

/// Names of every project still attached to `workspace_id`.
///
/// Plain and state-free so it can be unit tested without a full `AppState` —
/// the command below is just this check plus the actual registry mutation.
fn projects_referencing(projects: &HashMap<Uuid, Project>, workspace_id: Uuid) -> Vec<String> {
    projects
        .values()
        .filter(|p| p.scope.workspace_id() == Some(workspace_id))
        .map(|p| p.name.clone())
        .collect()
}

#[tauri::command]
pub async fn delete_workspace(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    workspace_id: String,
) -> Result<bool, String> {
    let workspace_id = Uuid::parse_str(&workspace_id).map_err(|e| e.to_string())?;

    // Deleting the workspace out from under a project that still points at it
    // would leave that project's `scope` referencing a workspace id nothing
    // can resolve anymore. Reject the deletion instead of guessing at a
    // cascade — the caller can re-scope or delete those projects first.
    // Held across the registry mutation below: releasing it right after the
    // check would leave a window where a project can attach to this
    // workspace before the removal lands, recreating the dangling reference
    // this check exists to prevent. Lock order: any path that holds both the
    // projects lock and the workspace registry lock takes projects first,
    // then the registry (`attach_project_to_workspace` does the same), so
    // the two cannot deadlock.
    let projects = state.project.projects.read().await;
    let referencing = projects_referencing(&projects, workspace_id);
    if !referencing.is_empty() {
        return Err(format!(
            "Cannot delete workspace: {} project(s) still reference it: {}",
            referencing.len(),
            referencing.join(", ")
        ));
    }

    // The "no project references it" check above read memory, which may follow
    // a detach that is published but unconfirmed. Removing the workspace is a
    // durable fact in another file, so it waits for that to be confirmed.
    state
        .storage
        .confirm_config_durable()
        .map_err(|e| e.describe("confirm the project configuration before removing the workspace"))?;
    let mut registry = state.workspace.registry.write().await;
    registry.remove(&workspace_id).map_err(|e| e.describe("remove the workspace"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AgentConfig, AgentType, ProjectScope};
    use crate::config::paths::StateLocation;

    fn project(name: &str, scope: ProjectScope) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            repository_id: None,
            scope,
            state_location: StateLocation::External,
            base: None,
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
            removed_at: None,
        }
    }

    #[test]
    fn a_removed_member_still_blocks_deleting_its_workspace() {
        // Removal keeps the membership, so the reference must still be reported.
        let workspace_id = Uuid::new_v4();
        let mut removed = project("removed-member", ProjectScope::InWorkspace { workspace_id });
        removed.removed_at = Some(chrono::Utc::now());
        let mut projects = HashMap::new();
        projects.insert(removed.id, removed);

        assert_eq!(projects_referencing(&projects, workspace_id), vec!["removed-member".to_string()]);
    }

    #[test]
    fn no_projects_reference_an_unrelated_or_standalone_workspace() {
        let workspace_id = Uuid::new_v4();
        let other_workspace_id = Uuid::new_v4();
        let mut projects = HashMap::new();
        let standalone = project("standalone", ProjectScope::Standalone);
        let other = project(
            "other-workspace",
            ProjectScope::InWorkspace {
                workspace_id: other_workspace_id,
            },
        );
        projects.insert(standalone.id, standalone);
        projects.insert(other.id, other);

        assert!(projects_referencing(&projects, workspace_id).is_empty());
    }

    #[test]
    fn projects_still_in_the_workspace_are_reported_by_name() {
        let workspace_id = Uuid::new_v4();
        let mut projects = HashMap::new();
        let attached = project(
            "board",
            ProjectScope::InWorkspace { workspace_id },
        );
        projects.insert(attached.id, attached);

        let referencing = projects_referencing(&projects, workspace_id);
        assert_eq!(referencing, vec!["board".to_string()]);
    }

    /// Paths and a registry inside one tempdir, so a test never touches the
    /// real application folders.
    fn fixture() -> (tempfile::TempDir, AppPaths, RwLock<WorkspaceRegistry>) {
        let temp = tempfile::TempDir::new().unwrap();
        let paths = AppPaths::with_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
            temp.path().join("run"),
        );
        let registry = WorkspaceRegistry::load_from(paths.workspaces_file()).unwrap();
        (temp, paths, RwLock::new(registry))
    }

    #[tokio::test]
    async fn a_workspace_without_a_folder_gets_its_managed_root_at_creation() {
        let (_temp, paths, registry) = fixture();

        let workspace = create_workspace_core(&registry, &paths, "  Client  ".into(), None).await.unwrap();

        assert_eq!(workspace.name, "Client");
        assert!(workspace.root_path.is_none(), "the record keeps no custom root");
        let managed = paths.workspace_managed_root(workspace.id);
        assert!(managed.is_dir(), "the managed root exists at the application-owned location");
        assert!(managed.starts_with(paths.data_dir()));
        let info = workspace_root_core(&registry, &paths, workspace.id).await.unwrap();
        assert_eq!(info.path, managed.display().to_string());
        assert!(!info.custom && info.exists);
    }

    #[tokio::test]
    async fn a_custom_root_is_validated_and_never_created_or_moved() {
        let (temp, paths, registry) = fixture();
        let folder = temp.path().join("shared");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = create_workspace_core(&registry, &paths, "Shared".into(), None).await.unwrap();
        let managed = paths.workspace_managed_root(workspace.id);

        let changed = set_workspace_root_core(&registry, &paths, workspace.id, Some(folder.display().to_string()))
            .await
            .unwrap();

        assert!(changed.root_path.is_some());
        assert!(managed.is_dir(), "the previous managed folder is left in place");
        let missing = temp.path().join("never-made");
        let refusal = set_workspace_root_core(&registry, &paths, workspace.id, Some(missing.display().to_string()))
            .await
            .expect_err("a root that does not exist is refused");
        assert!(refusal.contains("Invalid workspace path"), "{refusal}");
        assert!(!missing.exists(), "a refused root is never created");
    }

    #[tokio::test]
    async fn a_folder_already_owned_by_another_workspace_is_refused_but_kept_by_its_owner() {
        let (temp, paths, registry) = fixture();
        let folder = temp.path().join("shared");
        std::fs::create_dir_all(&folder).unwrap();
        let first = create_workspace_core(&registry, &paths, "First".into(), Some(folder.display().to_string()))
            .await
            .unwrap();
        let second = create_workspace_core(&registry, &paths, "Second".into(), None).await.unwrap();

        let refusal = set_workspace_root_core(&registry, &paths, second.id, Some(folder.display().to_string()))
            .await
            .expect_err("one folder cannot be two Workspaces' roots");
        assert!(refusal.contains("First"), "{refusal}");
        assert!(registry.read().await.get(&first.id).unwrap().root_path.is_some());
        // The owner may set its own root again without tripping over itself.
        set_workspace_root_core(&registry, &paths, first.id, Some(folder.display().to_string()))
            .await
            .expect("re-setting a Workspace's own root is accepted");
    }

    /// A Workspace without a custom root reads its managed folder, so that
    /// folder is owned too: another Workspace cannot claim it as a custom root.
    #[tokio::test]
    async fn a_folder_that_is_another_workspaces_managed_folder_is_refused() {
        let (temp, paths, registry) = fixture();
        let owner = create_workspace_core(&registry, &paths, "Owner".into(), None).await.unwrap();
        let managed = paths.workspace_managed_root(owner.id);
        // Refusals name the canonical folder (a Windows verbatim path differs from the raw one).
        let canonical = managed.canonicalize().unwrap().display().to_string();
        let other = create_workspace_core(&registry, &paths, "Other".into(), None).await.unwrap();

        let refusal = set_workspace_root_core(&registry, &paths, other.id, Some(managed.display().to_string()))
            .await
            .expect_err("a managed folder read by another Workspace cannot become a custom root");
        assert!(refusal.contains("Owner"), "{refusal}");
        assert!(refusal.contains(&canonical), "{refusal}");
        let refusal = create_workspace_core(&registry, &paths, "Third".into(), Some(managed.display().to_string()))
            .await
            .expect_err("creating over another Workspace's managed folder is refused");
        assert!(refusal.message.contains("already registered"), "{}", refusal.message);
        assert!(refusal.message.contains(&canonical), "{}", refusal.message);
        assert!(refusal.workspace.is_none(), "nothing was created");

        // A distinct folder is still accepted for the same Workspace.
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        set_workspace_root_core(&registry, &paths, other.id, Some(elsewhere.display().to_string()))
            .await
            .expect("a folder no other Workspace reads is accepted");
    }

    /// Whitespace is part of a folder name on Unix: a root whose name ends in a
    /// space is that folder, not a trimmed path that does not exist.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_custom_root_keeps_whitespace_that_is_part_of_its_path() {
        let (temp, paths, registry) = fixture();
        let spaced = temp.path().join("client ");
        std::fs::create_dir_all(&spaced).unwrap();
        let workspace = create_workspace_core(&registry, &paths, "Spaced".into(), None).await.unwrap();

        let updated = set_workspace_root_core(&registry, &paths, workspace.id, Some(spaced.display().to_string()))
            .await
            .expect("a folder whose name ends in a space is a valid root");
        assert_eq!(
            updated.root_path.as_ref().map(|root| root.as_path().to_path_buf()),
            Some(spaced.canonicalize().unwrap())
        );
    }

    /// The data directory is reached through a symlink, so the managed folder
    /// has two spellings. The canonical one names the same folder and is refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_data_directory_cannot_hide_another_workspaces_managed_folder() {
        let temp = tempfile::TempDir::new().unwrap();
        let real_data = temp.path().join("real-data");
        std::fs::create_dir_all(&real_data).unwrap();
        let data_alias = temp.path().join("data");
        std::os::unix::fs::symlink(&real_data, &data_alias).unwrap();
        let paths = AppPaths::with_roots(
            temp.path().join("config"),
            data_alias,
            temp.path().join("cache"),
            temp.path().join("run"),
        );
        let registry = RwLock::new(WorkspaceRegistry::load_from(paths.workspaces_file()).unwrap());
        let owner = create_workspace_core(&registry, &paths, "Owner".into(), None).await.unwrap();
        let other = create_workspace_core(&registry, &paths, "Other".into(), None).await.unwrap();
        let canonical_managed = real_data.join("workspaces").join(owner.id.to_string());

        let refusal = set_workspace_root_core(
            &registry,
            &paths,
            other.id,
            Some(canonical_managed.display().to_string()),
        )
        .await
        .expect_err("the canonical spelling of a managed folder is the same folder");
        assert!(refusal.contains("Owner"), "{refusal}");
        assert!(registry.read().await.get(&other.id).unwrap().root_path.is_none());
    }

    #[tokio::test]
    async fn clearing_the_custom_root_returns_to_the_managed_root_without_deleting_anything() {
        let (temp, paths, registry) = fixture();
        let folder = temp.path().join("shared");
        std::fs::create_dir_all(&folder).unwrap();
        let workspace = create_workspace_core(&registry, &paths, "W".into(), Some(folder.display().to_string()))
            .await
            .unwrap();

        let cleared = set_workspace_root_core(&registry, &paths, workspace.id, None).await.unwrap();

        assert!(cleared.root_path.is_none());
        assert!(paths.workspace_managed_root(workspace.id).is_dir());
        assert!(folder.is_dir(), "the custom folder is never deleted");
    }

    /// Workspace W uses a custom root, so its managed folder is unowned. Another
    /// Workspace may then claim that managed folder. Clearing W's root would
    /// return W to the claimed folder, so it must be refused and nothing changes.
    #[tokio::test]
    async fn clearing_a_custom_root_is_refused_when_another_workspace_owns_the_managed_folder() {
        let (temp, paths, registry) = fixture();
        let custom = temp.path().join("custom");
        std::fs::create_dir_all(&custom).unwrap();
        let w = create_workspace_core(&registry, &paths, "W".into(), Some(custom.display().to_string()))
            .await
            .unwrap();
        let w_managed = paths.workspace_managed_root(w.id);
        std::fs::create_dir_all(&w_managed).unwrap();
        let other = create_workspace_core(&registry, &paths, "Other".into(), None).await.unwrap();
        set_workspace_root_core(&registry, &paths, other.id, Some(w_managed.display().to_string()))
            .await
            .expect("W's managed folder is unowned while W uses a custom root");

        let refusal = set_workspace_root_core(&registry, &paths, w.id, None)
            .await
            .expect_err("clearing W's root would give W a folder that Other already owns");

        assert!(refusal.contains("Other"), "{refusal}");
        let persisted = registry.read().await.get(&w.id).unwrap().root_path.clone();
        // The stored root is canonical (on Windows an extended-length path), so the
        // expected value is canonicalized too rather than compared with the temp spelling.
        assert_eq!(
            persisted.as_ref().map(|r| r.as_path().to_path_buf()),
            Some(custom.canonicalize().unwrap())
        );
        assert!(custom.is_dir(), "the custom folder is untouched");
    }

    /// The data directory is reached through a symlink, so the managed folder has two
    /// spellings: clearing must see that Other owns the same folder through either.
    #[cfg(unix)]
    #[tokio::test]
    async fn clearing_refuses_a_managed_folder_claimed_through_the_canonical_spelling() {
        let temp = tempfile::TempDir::new().unwrap();
        let real_data = temp.path().join("real-data");
        std::fs::create_dir_all(&real_data).unwrap();
        let data_alias = temp.path().join("data");
        std::os::unix::fs::symlink(&real_data, &data_alias).unwrap();
        let paths = AppPaths::with_roots(
            temp.path().join("config"),
            data_alias,
            temp.path().join("cache"),
            temp.path().join("run"),
        );
        let registry = RwLock::new(WorkspaceRegistry::load_from(paths.workspaces_file()).unwrap());
        let custom = temp.path().join("custom");
        std::fs::create_dir_all(&custom).unwrap();
        let w = create_workspace_core(&registry, &paths, "W".into(), Some(custom.display().to_string()))
            .await
            .unwrap();
        // Other claims W's managed folder through its canonical spelling; the data
        // directory alias names the same folder.
        let canonical_w_managed = real_data.join("workspaces").join(w.id.to_string());
        std::fs::create_dir_all(&canonical_w_managed).unwrap();
        let other = create_workspace_core(&registry, &paths, "Other".into(), None).await.unwrap();
        set_workspace_root_core(&registry, &paths, other.id, Some(canonical_w_managed.display().to_string()))
            .await
            .expect("W's managed folder is unowned while W uses a custom root");

        let refusal = set_workspace_root_core(&registry, &paths, w.id, None)
            .await
            .expect_err("the managed folder is Other's root through the alias");

        assert!(refusal.contains("Other"), "{refusal}");
    }

    #[test]
    fn canonical_with_missing_tail_resolves_the_existing_ancestor() {
        let temp = tempfile::TempDir::new().unwrap();
        let existing = temp.path().join("present");
        std::fs::create_dir_all(&existing).unwrap();
        let expected = existing.canonicalize().unwrap().join("a").join("b");

        assert_eq!(canonical_with_missing_tail(&existing.join("a").join("b")), expected);
    }

    #[tokio::test]
    async fn renaming_keeps_membership_and_root_and_refuses_an_empty_name() {
        let (_temp, paths, registry) = fixture();
        let workspace = create_workspace_core(&registry, &paths, "Old".into(), None).await.unwrap();

        let renamed = rename_workspace_core(&registry, workspace.id, " New name ".into()).await.unwrap();

        assert_eq!(renamed.name, "New name");
        assert_eq!(renamed.root_path, workspace.root_path);
        assert!(rename_workspace_core(&registry, workspace.id, "  ".into()).await.is_err());
    }

    #[tokio::test]
    async fn a_save_that_fails_before_publication_creates_nothing_and_reports_no_workspace() {
        let (_temp, paths, registry) = fixture();
        let _fault = slashit_durable_fs::fault::fail_at(
            slashit_durable_fs::Stage::Replace,
            std::io::ErrorKind::Other,
        );

        let failure = create_workspace_core(&registry, &paths, "Client".into(), None)
            .await
            .expect_err("the save did not happen");

        assert!(
            failure.workspace.is_none(),
            "nothing was created, so there is nothing to show"
        );
        assert!(
            registry.read().await.all().next().is_none(),
            "memory did not change"
        );
    }

    #[tokio::test]
    async fn an_unconfirmed_save_reports_the_created_workspace_once_and_reconciles_without_a_second_one(
    ) {
        let (_temp, paths, registry) = fixture();
        let path = paths.workspaces_file();
        let fault = slashit_durable_fs::fault::fail_at(
            slashit_durable_fs::Stage::SyncDirectory,
            std::io::ErrorKind::Other,
        );

        let failure = create_workspace_core(&registry, &paths, "Client".into(), None)
            .await
            .expect_err("durability is unconfirmed");
        drop(fault);

        let created = failure
            .workspace
            .expect("the registry holds the Workspace, so it must be reported");
        assert!(
            registry.read().await.get(&created.id).is_some(),
            "memory follows the file"
        );
        let reloaded = WorkspaceRegistry::load_from(path).unwrap();
        assert!(
            reloaded.get(&created.id).is_some(),
            "a restart sees the same Workspace"
        );

        let listed: Vec<_> = registry.read().await.all().map(|w| w.id).collect();
        assert_eq!(
            listed,
            vec![created.id],
            "a reload lists exactly one Workspace"
        );
    }
}
