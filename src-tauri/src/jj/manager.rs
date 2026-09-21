use anyhow::{Context, Result};
use std::path::Path;

pub struct JjManager;

impl JjManager {
    pub fn new() -> Self {
        Self
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
