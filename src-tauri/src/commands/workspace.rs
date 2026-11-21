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
    match root_path.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
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

/// Creates a Workspace. Without a custom root it gets its managed root at once,
/// so the Workspace has an execution context from the start.
/// The Workspace that already reads `root`: its custom root, or its managed
/// folder when it has none. `except` is the Workspace being changed.
fn root_owner<'a>(
    registry: &'a WorkspaceRegistry,
    paths: &AppPaths,
    root: &std::path::Path,
    except: Option<Uuid>,
) -> Option<&'a Workspace> {
    registry.all().find(|w| {
        if Some(w.id) == except {
            return false;
        }
        let effective = match &w.root_path {
            Some(custom) => custom.as_path().to_path_buf(),
            None => paths.workspace_managed_root(w.id),
        };
        effective.as_path() == root
    })
}

pub(crate) async fn create_workspace_core(
    registry: &RwLock<WorkspaceRegistry>,
    paths: &AppPaths,
    name: String,
    root_path: Option<String>,
) -> Result<Workspace, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("Workspace name is required".to_string());
    }
    let root = custom_root(root_path)?;
    let mut registry = registry.write().await;
    if let Some(root) = &root {
        if root_owner(&registry, paths, root.as_path(), None).is_some() {
            return Err(format!(
                "A workspace is already registered for {}",
                root.as_path().display()
            ));
        }
    }
    let workspace = Workspace::new(name, root);
    if workspace.root_path.is_none() {
        ensure_managed_root(paths, workspace.id)?;
    }
    registry.upsert(workspace.clone()).map_err(|e| e.describe("save the workspace"))?;
    Ok(workspace)
}

#[tauri::command]
pub async fn create_workspace(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    name: String,
    root_path: Option<String>,
) -> Result<Workspace, String> {
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
    if let Some(root) = &root {
        let owner = root_owner(&registry, paths, root.as_path(), Some(workspace_id));
        if let Some(owner) = owner {
            return Err(format!(
                "The folder {} already belongs to the Workspace \"{}\"",
                root.as_path().display(),
                owner.name
            ));
        }
    } else {
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

    #[tokio::test]
    async fn renaming_keeps_membership_and_root_and_refuses_an_empty_name() {
        let (_temp, paths, registry) = fixture();
        let workspace = create_workspace_core(&registry, &paths, "Old".into(), None).await.unwrap();

        let renamed = rename_workspace_core(&registry, workspace.id, " New name ".into()).await.unwrap();

        assert_eq!(renamed.name, "New name");
        assert_eq!(renamed.root_path, workspace.root_path);
        assert!(rename_workspace_core(&registry, workspace.id, "  ".into()).await.is_err());
    }
}
