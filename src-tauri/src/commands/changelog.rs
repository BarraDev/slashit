use crate::domain::changelog::{ChangelogSource, ChangelogOptions, GitCommit};

#[derive(Clone)]
pub struct ChangelogState;

impl ChangelogState {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ChangelogState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn generate_changelog(
    source: ChangelogSource,
    _options: ChangelogOptions,
) -> Result<String, String> {
    match source {
        ChangelogSource::CompletedTasks => {
            Ok("# Changelog\n\n## Completed Tasks\n\nNo completed tasks yet.".to_string())
        }
        ChangelogSource::GitHistory => {
            Ok("# Changelog\n\n## Git History\n\nNo git history available.".to_string())
        }
        ChangelogSource::BranchComparison => {
            Ok("# Changelog\n\n## Branch Comparison\n\nNo branch comparison available.".to_string())
        }
    }
}

#[tauri::command]
pub async fn get_git_history(
    _repo_path: String,
    _count: usize,
    _include_merges: bool,
) -> Result<Vec<GitCommit>, String> {
    Ok(vec![])
}

#[tauri::command]
pub async fn compare_branches(
    _repo_path: String,
    _base: String,
    _compare: String,
) -> Result<Vec<GitCommit>, String> {
    Ok(vec![])
}
