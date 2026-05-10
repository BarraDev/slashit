use anyhow::{Context, Result};
use std::path::Path;

pub struct WorkspaceManager;

impl WorkspaceManager {
    pub fn new() -> Self {
        Self
    }

    pub fn create(&self, project_path: &Path, name: &str, base_revision: Option<&str>) -> Result<String> {
        let workspace_path = project_path.join(name);

        let mut args = vec!("workspace", "add", "--name", name);
        if let Some(revision) = base_revision {
            args = vec!("workspace", "add", "--name", name, revision);
        }

        let output = std::process::Command::new("jj")
            .args(args)
            .current_dir(project_path)
            .output()
            .context("Failed to create workspace")?;

        if !output.status.success() {
            anyhow::bail!("jj workspace add failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        Ok(workspace_path.to_string_lossy().to_string())
    }

    pub fn list(&self, project_path: &Path) -> Result<Vec<String>> {
        let output = std::process::Command::new("jj")
            .args(["workspace", "list"])
            .current_dir(project_path)
            .output()
            .context("Failed to list workspaces")?;

        if !output.status.success() {
            anyhow::bail!("jj workspace list failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let workspaces: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();

        Ok(workspaces)
    }

    pub fn remove(&self, project_path: &Path, name: &str) -> Result<()> {
        let output = std::process::Command::new("jj")
            .args(["workspace", "forget", name])
            .current_dir(project_path)
            .output()
            .context("Failed to remove workspace")?;

        if !output.status.success() {
            anyhow::bail!("jj workspace forget failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        Ok(())
    }
}

impl Default for WorkspaceManager {
    fn default() -> Self {
        Self::new()
    }
}
