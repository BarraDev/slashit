use crate::domain::file::{FileItem, FileInfo};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use chrono::{DateTime, Utc};
use tauri_plugin_dialog::DialogExt;

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

#[tauri::command]
pub async fn list_files(path: String) -> Result<Vec<FileItem>, String> {
    let path = PathBuf::from(&path);

    if !path.exists() {
        return Err(format!("Path does not exist: {}", path.display()));
    }

    let mut items = Vec::new();

    if path.is_dir() {
        let entries = fs::read_dir(&path)
            .map_err(|e| format!("Failed to read directory: {}", e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
            let metadata = entry.metadata().map_err(|e| format!("Failed to read metadata: {}", e))?;

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
pub async fn read_file(path: String) -> Result<String, String> {
    fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read file: {}", e))
}

#[tauri::command]
pub async fn write_file(path: String, content: String) -> Result<(), String> {
    fs::write(&path, content)
        .map_err(|e| format!("Failed to write file: {}", e))
}

#[tauri::command]
pub async fn search_files(query: String, path: String) -> Result<Vec<FileItem>, String> {
    let base_path = PathBuf::from(&path);

    if !base_path.exists() {
        return Err(format!("Path does not exist: {}", base_path.display()));
    }

    let mut results = Vec::new();
    let query_lower = query.to_lowercase();

    for entry in WalkDir::new(&base_path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let file_name = entry.file_name().to_string_lossy();
        if file_name.to_lowercase().contains(&query_lower) {
            let metadata = entry.metadata().ok();
            let modified = metadata.as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0));

            results.push(FileItem {
                name: file_name.to_string(),
                path: entry.path().to_string_lossy().to_string(),
                is_dir: metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false),
                size: metadata.and_then(|m| if m.is_file() { Some(m.len()) } else { None }),
                modified,
            });
        }
    }

    Ok(results)
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

#[tauri::command]
pub async fn get_file_info(path: String) -> Result<FileInfo, String> {
    let path_obj = Path::new(&path);

    let metadata = fs::metadata(path_obj)
        .map_err(|e| format!("Failed to get file metadata: {}", e))?;

    let created = metadata.created()
        .ok()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0))
        .unwrap_or_else(Utc::now);

    let modified = metadata.modified()
        .ok()
        .and_then(|t| DateTime::<Utc>::from_timestamp(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64, 0))
        .unwrap_or_else(Utc::now);

    let readonly = metadata.permissions().readonly();
    let permissions = if readonly { "r--" } else { "rw-" };

    Ok(FileInfo {
        path: path.clone(),
        size: metadata.len(),
        created,
        modified,
        permissions: permissions.to_string(),
    })
}
