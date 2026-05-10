use super::workspace::WorkspaceManager;
use anyhow::{Context, Result};
use std::path::Path;

pub struct JjManager {
    workspace_manager: WorkspaceManager,
}

impl JjManager {
    pub fn new() -> Self {
        Self {
            workspace_manager: WorkspaceManager::new(),
        }
    }

    pub fn create_workspace(
        &self,
        project_path: &Path,
        name: &str,
        base_revision: Option<&str>,
    ) -> Result<String> {
        self.workspace_manager
            .create(project_path, name, base_revision)
    }

    pub fn list_workspaces(&self, project_path: &Path) -> Result<Vec<String>> {
        self.workspace_manager.list(project_path)
    }

    pub fn remove_workspace(&self, project_path: &Path, name: &str) -> Result<()> {
        self.workspace_manager.remove(project_path, name)
    }

    pub fn get_status(&self, workspace_path: &Path) -> Result<JjStatus> {
        let output = std::process::Command::new("jj")
            .args(["status", "--template", "json"])
            .current_dir(workspace_path)
            .output()
            .context("Failed to get jj status")?;

        if !output.status.success() {
            anyhow::bail!("jj status failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        serde_json::from_slice(&output.stdout).context("Failed to parse jj status")
    }

    pub fn new_change(&self, workspace_path: &Path, description: &str) -> Result<String> {
        let output = std::process::Command::new("jj")
            .args(["new", "--message", description])
            .current_dir(workspace_path)
            .output()
            .context("Failed to create new change")?;

        if !output.status.success() {
            anyhow::bail!("jj new failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let change_id = String::from_utf8(output.stdout)
            .context("Failed to parse change id")?
            .trim()
            .to_string();

        Ok(change_id)
    }

    pub fn describe_change(&self, workspace_path: &Path, change_id: &str, description: &str) -> Result<()> {
        let output = std::process::Command::new("jj")
            .args(["describe", change_id, "--message", description])
            .current_dir(workspace_path)
            .output()
            .context("Failed to describe change")?;

        if !output.status.success() {
            anyhow::bail!("jj describe failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        Ok(())
    }

    pub fn abandon_change(&self, workspace_path: &Path, change_id: &str) -> Result<()> {
        let output = std::process::Command::new("jj")
            .args(["abandon", change_id])
            .current_dir(workspace_path)
            .output()
            .context("Failed to abandon change")?;

        if !output.status.success() {
            anyhow::bail!("jj abandon failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        Ok(())
    }

    pub fn diff(&self, workspace_path: &Path) -> Result<String> {
        Self::run_with_fallback(
            workspace_path,
            &["diff", "--git"],
            &["diff", "HEAD"],
            &["show", "HEAD", "--format=", "--patch"],
        )
    }

    pub fn diff_stat(&self, workspace_path: &Path) -> Result<String> {
        Self::run_with_fallback(
            workspace_path,
            &["diff", "--stat"],
            &["diff", "--stat", "HEAD"],
            &["show", "HEAD", "--format=", "--stat"],
        )
    }

    /// Try jj first, then git uncommitted, then git last commit.
    /// Returns the first non-empty successful output, or empty string.
    fn run_with_fallback(
        workspace_path: &Path,
        jj_args: &[&str],
        git_uncommitted_args: &[&str],
        git_show_args: &[&str],
    ) -> Result<String> {
        let commands: &[(&str, &[&str])] = &[
            ("jj", jj_args),
            ("git", git_uncommitted_args),
            ("git", git_show_args),
        ];

        for (cmd, args) in commands {
            if let Ok(output) = std::process::Command::new(cmd)
                .args(*args)
                .current_dir(workspace_path)
                .output()
            {
                if output.status.success() {
                    let text = String::from_utf8(output.stdout)
                        .context("Failed to parse command output")?;
                    if !text.trim().is_empty() {
                        return Ok(text);
                    }
                }
            }
        }

        Ok(String::new())
    }

    pub fn git_export(&self, workspace_path: &Path) -> Result<()> {
        let output = std::process::Command::new("jj")
            .args(["git", "export"])
            .current_dir(workspace_path)
            .output()
            .context("Failed to export to git")?;

        if !output.status.success() {
            anyhow::bail!("jj git export failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        Ok(())
    }
}

impl Default for JjManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JjStatus {
    pub current_change_id: Option<String>,
    pub pending_changes: bool,
    pub conflicted: bool,
}
