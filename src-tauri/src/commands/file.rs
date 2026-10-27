// SPDX-License-Identifier: MPL-2.0
use crate::domain::file::FileItem;
use std::fs;
use std::path::{Path, PathBuf};
use chrono::{DateTime, Utc};
use tauri_plugin_dialog::DialogExt;
use uuid::Uuid;

#[derive(Clone)]
pub struct FileState;

impl FileState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for FileState {
    fn default() -> Self {
        Self::new()
    }
}

/// The folder a Project's file commands may read: its repository path, resolved
/// from the backend's own records, never from what the caller sends.
async fn project_root_for(
    state: &tauri::State<'_, crate::AppState>,
    project_id: &str,
) -> Result<PathBuf, String> {
    let id = Uuid::parse_str(project_id).map_err(|e| e.to_string())?;
    // The locks are held only to copy the path out. Canonicalizing touches the
    // filesystem, which can block on a slow mount, and writers must not wait
    // behind it.
    let local_path = {
        let projects = state.project.projects.read().await;
        let project = projects.get(&id).ok_or("Project not found")?;
        let repositories = state.repository.repositories.read().await;
        project
            .repository_path(&repositories)
            .ok_or("This Project has no folder to read from.")?
            .to_string()
    };
    resolve_root(&local_path)
}

/// Canonical form of a Project's root, so containment can be a prefix check.
pub(crate) fn resolve_root(local_path: &str) -> Result<PathBuf, String> {
    Path::new(local_path)
        .canonicalize()
        .map_err(|e| format!("The Project folder is not available: {e}"))
}

/// Resolves `path` and requires it to lie inside `root`.
///
/// The path must be absolute, and it is canonicalized before the check, so a
/// symlink inside the Project that points elsewhere is refused. The check is
/// made once, before the read: a link swapped in after it is not covered.
///
/// The contract is to refuse paths the webview asks for. It is not a sandbox
/// against a process running as the same user with write access to the Project:
/// that process can already read every file the app can, so a swap made while
/// this check runs gives it nothing new.
pub(crate) fn within_root(root: &Path, path: &str) -> Result<PathBuf, String> {
    let requested = Path::new(path);
    if !requested.is_absolute() {
        return Err("A file path must be absolute.".to_string());
    }
    let resolved = requested
        .canonicalize()
        .map_err(|e| format!("Path does not exist or cannot be read: {} ({e})", requested.display()))?;
    if resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(format!("{} is outside this Project's folder.", requested.display()))
    }
}

/// The metadata a listing shows for one directory entry. `DirEntry::metadata`
/// does not follow links, so a link is described by itself by default. A link
/// whose target lies inside the Project is described by that target, which is
/// what the read commands open. A link that points outside keeps its own
/// metadata, so the target's type and size never reach the listing.
fn listed_metadata(root: &Path, entry: &fs::DirEntry) -> std::io::Result<fs::Metadata> {
    if entry.file_type()?.is_symlink() && within_root(root, &entry.path().to_string_lossy()).is_ok() {
        if let Ok(target) = fs::metadata(entry.path()) {
            return Ok(target);
        }
    }
    entry.metadata()
}

#[tauri::command]
pub async fn list_files(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    path: String,
) -> Result<Vec<FileItem>, String> {
    let root = project_root_for(&state, &project_id).await?;
    let path = within_root(&root, &path)?;

    let mut items = Vec::new();

    if path.is_dir() {
        let entries = fs::read_dir(&path)
            .map_err(|e| format!("Failed to read directory: {}", e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
            let metadata = listed_metadata(&root, &entry).map_err(|e| format!("Failed to read metadata: {}", e))?;

            let modified = metadata
                .modified()
                .ok()
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0));

            items.push(FileItem {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                is_dir: metadata.is_dir(),
                size: if metadata.is_file() { Some(metadata.len()) } else { None },
                modified,
            });
        }

        items.sort_by(|a, b| {
            if a.is_dir != b.is_dir {
                b.is_dir.cmp(&a.is_dir)
            } else {
                a.name.cmp(&b.name)
            }
        });
    }

    Ok(items)
}

#[tauri::command]
pub async fn read_file(
    state: tauri::State<'_, crate::AppState>,
    project_id: String,
    path: String,
) -> Result<String, String> {
    let root = project_root_for(&state, &project_id).await?;
    let path = within_root(&root, &path)?;
    fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read file: {}", e))
}

#[tauri::command]
pub async fn pick_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    
    app.dialog()
        .file()
        .set_title("Select Repository Folder")
        .pick_folder(move |folder_path| {
            let result = folder_path.map(|p| p.to_string());
            let _ = tx.send(result);
        });
    
    match rx.await {
        Ok(Some(path)) => Ok(Some(path)),
        Ok(None) => Ok(None), // User cancelled
        Err(_) => Err("Dialog channel closed unexpectedly".to_string()),
    }
}

/// Whether `path` is itself the root of a git repository.
///
/// Deliberately only looks at `path/.git`: a nested repository under an
/// ancestor repository is still a valid, independent repository for this
/// check, and walking up to an ancestor would make an intentional nested
/// checkout look uninitialized. `.exists()` (not `.is_dir()`) is required
/// because a linked git worktree's `.git` is a *file* pointing at the real
/// gitdir, not a directory.
#[cfg(test)]
pub(crate) fn is_git_repo_root(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Where `path` sits relative to git's own notion of repository boundaries.
///
/// Uses `git rev-parse --show-toplevel` instead of walking `.git` ourselves:
/// git already knows how to find the real root from any subdirectory, and
/// already treats a worktree's `.git` file the same as an ordinary `.git`
/// directory. Re-deriving that logic by hand (e.g. via `.git.is_dir()`)
/// would get the linked-worktree case wrong.
pub(crate) enum GitLocation {
    /// `path` is itself the toplevel of a repository (ordinary root, an
    /// intentional nested repo, or a linked worktree root).
    RepoRoot,
    /// `path` is a plain subdirectory of `root`, with no `.git` of its own.
    InsideRepo { root: PathBuf },
    /// `path` is not inside any git repository.
    NotARepo,
}

pub(crate) async fn git_location(path: &Path) -> Result<GitLocation, String> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(path)
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        // git exits non-zero both for "not a repository" and for edge cases
        // (e.g. a bare repository) that this feature does not need to
        // distinguish: neither should be treated as an existing root.
        return Ok(GitLocation::NotARepo);
    }

    let toplevel = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canonical_toplevel = toplevel.canonicalize().unwrap_or(toplevel);

    if canonical_path == canonical_toplevel {
        Ok(GitLocation::RepoRoot)
    } else {
        Ok(GitLocation::InsideRepo { root: canonical_toplevel })
    }
}

/// What the frontend should tell the user about a picked folder, before they
/// submit the create-project form.
///
/// This is UX only, not a safety boundary: `create_repository` re-derives
/// the same classification at execution time, because this result can be
/// stale by the time the user submits (see `ensure_git_initialized`).
#[derive(serde::Serialize)]
pub struct GitDetection {
    /// True if `path` is already backed by git in any sense (its own root,
    /// or inside an ancestor repository) — used to hide the "Initialize git
    /// repository" checkbox, since offering to `git init` here is either
    /// redundant or, for the ancestor case, the accidental-nested-repo bug
    /// this type exists to prevent.
    pub is_git_repo: bool,
    /// Set when `path` is inside an ancestor repository rather than being a
    /// root itself, so the UI can name the real root instead of implying
    /// `path` will become one.
    pub ancestor_root: Option<String>,
}

#[tauri::command]
pub async fn check_is_git_repo(path: String) -> Result<GitDetection, String> {
    match git_location(&PathBuf::from(&path)).await? {
        GitLocation::RepoRoot => Ok(GitDetection { is_git_repo: true, ancestor_root: None }),
        GitLocation::InsideRepo { root } => Ok(GitDetection {
            is_git_repo: true,
            ancestor_root: Some(root.to_string_lossy().to_string()),
        }),
        GitLocation::NotARepo => Ok(GitDetection { is_git_repo: false, ancestor_root: None }),
    }
}

#[cfg(test)]
mod git_location_tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be on PATH for these tests")
    }

    fn init_committed_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        assert!(git(dir, &["init", "-q"]).status.success());
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        std::fs::write(dir.join("f"), "hi").unwrap();
        git(dir, &["add", "f"]);
        assert!(git(dir, &["commit", "-q", "-m", "init"]).status.success());
    }

    #[tokio::test]
    async fn repo_root_is_recognized_as_its_own_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        init_committed_repo(&repo);

        assert!(matches!(git_location(&repo).await.unwrap(), GitLocation::RepoRoot));
    }

    #[tokio::test]
    async fn child_with_no_own_git_is_inside_ancestor_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        init_committed_repo(&repo);
        let child = repo.join("child");
        std::fs::create_dir_all(&child).unwrap();

        match git_location(&child).await.unwrap() {
            GitLocation::InsideRepo { root } => {
                assert_eq!(root, repo.canonicalize().unwrap());
            }
            GitLocation::RepoRoot => panic!("child must not be classified as a repo root"),
            GitLocation::NotARepo => panic!("child must not be classified as outside any repo"),
        }
    }

    #[tokio::test]
    async fn path_outside_any_repository_is_not_a_repo() {
        let temp = tempfile::TempDir::new().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        assert!(matches!(git_location(&outside).await.unwrap(), GitLocation::NotARepo));
    }

    #[tokio::test]
    async fn explicit_nested_repository_is_recognized_as_its_own_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        init_committed_repo(&repo);
        let nested = repo.join("nested");
        init_committed_repo(&nested);

        assert!(matches!(git_location(&nested).await.unwrap(), GitLocation::RepoRoot));
    }

    #[tokio::test]
    async fn linked_worktree_with_git_file_is_recognized_as_its_own_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        init_committed_repo(&repo);
        let worktree = temp.path().join("worktree");
        assert!(
            git(&repo, &["worktree", "add", "-b", "wt-branch", worktree.to_str().unwrap()])
                .status
                .success()
        );

        assert!(
            worktree.join(".git").is_file(),
            "a linked worktree's .git must be a file, not a directory, for this test to be meaningful"
        );
        assert!(matches!(git_location(&worktree).await.unwrap(), GitLocation::RepoRoot));
    }

    #[tokio::test]
    async fn check_is_git_repo_reports_truthful_ancestor_root_for_a_child_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        init_committed_repo(&repo);
        let child = repo.join("child");
        std::fs::create_dir_all(&child).unwrap();

        let detection = check_is_git_repo(child.to_string_lossy().to_string()).await.unwrap();
        assert!(detection.is_git_repo, "a child of a repository is still git-backed");
        assert_eq!(
            detection.ancestor_root.as_deref(),
            Some(repo.canonicalize().unwrap().to_string_lossy().as_ref()),
            "the detection must name the real root, not silently claim the child is one"
        );
    }

    #[tokio::test]
    async fn check_is_git_repo_on_nonexistent_path_returns_err_not_panic() {
        let result = check_is_git_repo("/nonexistent/path/that/does/not/exist".to_string()).await;
        assert!(result.is_err(), "an inaccessible path must be reported as an error, not silently classified");
    }
}


#[cfg(test)]
mod containment_tests {
    use super::*;
    use tempfile::TempDir;

    /// Two Projects side by side, each with one file, and a file outside both.
    fn fixture() -> (TempDir, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let alpha = temp.path().join("alpha");
        let beta = temp.path().join("beta");
        fs::create_dir_all(alpha.join("docs")).unwrap();
        fs::create_dir_all(&beta).unwrap();
        fs::write(alpha.join("docs/notes.md"), "alpha").unwrap();
        fs::write(beta.join("beta.md"), "beta").unwrap();
        fs::write(temp.path().join("outside.txt"), "outside").unwrap();
        let alpha = resolve_root(&alpha.to_string_lossy()).unwrap();
        let beta = resolve_root(&beta.to_string_lossy()).unwrap();
        (temp, alpha, beta)
    }

    #[test]
    fn a_file_and_the_root_itself_inside_the_project_are_allowed() {
        let (_temp, alpha, _beta) = fixture();
        let notes = alpha.join("docs/notes.md");
        assert_eq!(within_root(&alpha, &notes.to_string_lossy()).unwrap(), notes.canonicalize().unwrap());
        assert_eq!(within_root(&alpha, &alpha.to_string_lossy()).unwrap(), alpha);
    }

    #[test]
    fn a_file_of_another_project_is_refused() {
        let (_temp, alpha, beta) = fixture();
        let beta_file = beta.join("beta.md");
        let err = within_root(&alpha, &beta_file.to_string_lossy()).unwrap_err();
        assert!(err.contains("outside this Project"), "{err}");
    }

    #[test]
    fn an_absolute_path_outside_every_project_is_refused() {
        let (temp, alpha, _beta) = fixture();
        let outside = temp.path().join("outside.txt");
        assert!(within_root(&alpha, &outside.to_string_lossy()).is_err());
    }

    #[test]
    fn traversal_out_of_the_project_is_refused() {
        let (temp, alpha, _beta) = fixture();
        let escaped = alpha.join("..").join("outside.txt");
        let err = within_root(&alpha, &escaped.to_string_lossy()).unwrap_err();
        assert!(err.contains("outside this Project"), "{err}");
        assert!(temp.path().join("outside.txt").exists());
    }

    #[test]
    fn a_relative_path_is_refused_rather_than_resolved_against_the_process_directory() {
        let (_temp, alpha, _beta) = fixture();
        let err = within_root(&alpha, "docs/notes.md").unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn a_missing_path_is_reported_as_unreadable() {
        let (_temp, alpha, _beta) = fixture();
        let missing = alpha.join("nope.md");
        assert!(within_root(&alpha, &missing.to_string_lossy()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_project_that_points_outside_is_refused() {
        let (temp, alpha, _beta) = fixture();
        let link = alpha.join("escape.txt");
        std::os::unix::fs::symlink(temp.path().join("outside.txt"), &link).unwrap();
        let err = within_root(&alpha, &link.to_string_lossy()).unwrap_err();
        assert!(err.contains("outside this Project"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_directory_pointing_outside_cannot_expose_its_contents() {
        let (temp, alpha, _beta) = fixture();
        let link = alpha.join("linked");
        std::os::unix::fs::symlink(temp.path(), &link).unwrap();
        assert!(within_root(&alpha, &link.join("outside.txt").to_string_lossy()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_project_that_points_inside_is_allowed() {
        let (_temp, alpha, _beta) = fixture();
        let link = alpha.join("alias.md");
        std::os::unix::fs::symlink(alpha.join("docs/notes.md"), &link).unwrap();
        let resolved = within_root(&alpha, &link.to_string_lossy()).unwrap();
        assert_eq!(resolved, alpha.join("docs/notes.md").canonicalize().unwrap());
    }

    #[cfg(unix)]
    fn listed(root: &Path, name: &str) -> fs::Metadata {
        let entry = fs::read_dir(root)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.file_name() == name)
            .unwrap();
        listed_metadata(root, &entry).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_directory_inside_the_project_is_listed_as_that_directory() {
        let (_temp, alpha, _beta) = fixture();
        std::os::unix::fs::symlink(alpha.join("docs"), alpha.join("docs-alias")).unwrap();
        assert!(listed(&alpha, "docs-alias").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_pointing_outside_the_project_does_not_list_its_target() {
        let (temp, alpha, _beta) = fixture();
        std::os::unix::fs::symlink(temp.path(), alpha.join("up")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("outside.txt"), alpha.join("escape.txt")).unwrap();
        for name in ["up", "escape.txt"] {
            let metadata = listed(&alpha, name);
            assert!(!metadata.is_dir(), "{name} listed as a directory of another Project");
            assert!(!metadata.is_file(), "{name} listed with a target's size");
        }
    }
}
