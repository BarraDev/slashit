// SPDX-License-Identifier: MPL-2.0
//! Desktop commands for portable Project export and import.
//!
//! The dialog commands only choose a path. Preview reads a file and writes
//! nothing. Only `apply_import`, called after the user confirms, changes state.

use serde::Deserialize;

use super::project_service::invoke_checked;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ExportOutcome {
    pub path: String,
    pub project_id: String,
    pub tasks: usize,
    pub bytes: usize,
    pub durable: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PreviewProject {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PreviewRepository {
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PreviewCounts {
    pub tasks: u32,
    pub dependencies: u32,
    pub activity_entries: u32,
    pub review_entries: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PreviewConflict {
    pub kind: String,
    pub id: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PreviewNormalization {
    pub task_id: String,
    pub reason: String,
}

/// What a valid bundle would do. Mirrors `portability::preview::Report`; only
/// the fields the interface shows are read.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PreviewReport {
    pub project: PreviewProject,
    pub repository: Option<PreviewRepository>,
    pub counts: PreviewCounts,
    pub checksum: String,
    pub conflicts: Vec<PreviewConflict>,
    pub normalization: Vec<PreviewNormalization>,
    pub excluded: Vec<String>,
    pub importable: bool,
}

/// The result of Apply. Mirrors `commands::portability::ImportOutcome`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ImportOutcome {
    Applied {
        project_id: String,
        project_name: String,
        tasks: u32,
        normalization: Vec<PreviewNormalization>,
        repository_remote: Option<String>,
        durable: bool,
    },
    Refused {
        conflicts: Vec<PreviewConflict>,
    },
    ChecksumMismatch {
        message: String,
    },
    Invalid {
        message: String,
    },
    Failed {
        message: String,
    },
    Incomplete {
        message: String,
    },
}

/// How a result should look to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Error,
}

/// The headline and tone for an Apply result. Each outcome says exactly what
/// changed, so a refusal is never shown as a generic failure.
pub fn import_headline(outcome: &ImportOutcome) -> (Tone, String) {
    match outcome {
        ImportOutcome::Applied { durable: true, project_name, .. } => {
            (Tone::Success, format!("Imported \"{project_name}\"."))
        }
        ImportOutcome::Applied { project_name, .. } => (
            Tone::Warning,
            format!(
                "Imported \"{project_name}\", but this filesystem could not confirm the change. It may not survive a power loss. Restart SlashIt to confirm it."
            ),
        ),
        ImportOutcome::Refused { .. } => (
            Tone::Error,
            "The import was refused because this instance already has a Project or Task with the same identity. Nothing was changed.".to_string(),
        ),
        ImportOutcome::ChecksumMismatch { .. } => (
            Tone::Error,
            "The file changed after it was previewed. Nothing was changed. Preview the file again.".to_string(),
        ),
        ImportOutcome::Invalid { .. } => (
            Tone::Error,
            "The file could not be used as a SlashIt export. Nothing was changed.".to_string(),
        ),
        ImportOutcome::Failed { .. } => (
            Tone::Error,
            "The import did not complete, and nothing was kept from it.".to_string(),
        ),
        ImportOutcome::Incomplete { .. } => (
            Tone::Warning,
            "The import is incomplete and could not be undone in this session. Restart SlashIt before importing this file again.".to_string(),
        ),
    }
}

/// The detail text for an outcome, when the backend supplied one.
pub fn import_detail(outcome: &ImportOutcome) -> Option<String> {
    match outcome {
        ImportOutcome::ChecksumMismatch { message }
        | ImportOutcome::Invalid { message }
        | ImportOutcome::Failed { message }
        | ImportOutcome::Incomplete { message } => Some(message.clone()),
        ImportOutcome::Refused { conflicts } => Some(
            conflicts
                .iter()
                .map(|c| format!("{}: {}", c.kind, c.detail))
                .collect::<Vec<_>>()
                .join("; "),
        ),
        ImportOutcome::Applied { .. } => None,
    }
}

/// Whether the confirm control may be offered for a preview. Only a valid
/// bundle with no identity conflict may be applied.
pub fn can_confirm(preview: &PreviewReport) -> bool {
    preview.importable
}

pub async fn choose_export_destination(project_name: String) -> Result<Option<String>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "projectName": project_name }))
        .map_err(|e| e.to_string())?;
    let response = invoke_checked("choose_export_destination", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn export_project(project_id: String, path: String) -> Result<ExportOutcome, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "projectId": project_id,
        "path": path,
    }))
    .map_err(|e| e.to_string())?;
    let response = invoke_checked("export_project", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn choose_import_file() -> Result<Option<String>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).map_err(|e| e.to_string())?;
    let response = invoke_checked("choose_import_file", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn preview_import(path: String) -> Result<PreviewReport, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "path": path }))
        .map_err(|e| e.to_string())?;
    let response = invoke_checked("preview_import", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn apply_import(path: String, checksum: String) -> Result<ImportOutcome, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "path": path,
        "checksum": checksum,
    }))
    .map_err(|e| e.to_string())?;
    let response = invoke_checked("apply_import", args).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(importable: bool) -> PreviewReport {
        PreviewReport {
            project: PreviewProject { id: "p".into(), name: "Alpha".into() },
            repository: None,
            counts: PreviewCounts { tasks: 2, dependencies: 0, activity_entries: 5, review_entries: 1 },
            checksum: "abc".into(),
            conflicts: vec![],
            normalization: vec![],
            excluded: vec!["api_key".into()],
            importable,
        }
    }

    #[test]
    fn only_an_importable_preview_offers_the_confirm_control() {
        assert!(can_confirm(&preview(true)));
        assert!(!can_confirm(&preview(false)));
    }

    #[test]
    fn a_durable_apply_is_a_success_and_names_the_project() {
        let outcome = ImportOutcome::Applied {
            project_id: "p".into(),
            project_name: "Alpha".into(),
            tasks: 2,
            normalization: vec![],
            repository_remote: None,
            durable: true,
        };
        assert_eq!(import_headline(&outcome), (Tone::Success, "Imported \"Alpha\".".into()));
        assert_eq!(import_detail(&outcome), None);
    }

    #[test]
    fn a_non_durable_apply_is_shown_as_a_warning_not_a_success() {
        let outcome = ImportOutcome::Applied {
            project_id: "p".into(),
            project_name: "Alpha".into(),
            tasks: 2,
            normalization: vec![],
            repository_remote: None,
            durable: false,
        };
        let (tone, text) = import_headline(&outcome);
        assert_eq!(tone, Tone::Warning);
        assert!(text.contains("could not confirm"), "{text}");
    }

    #[test]
    fn each_refusal_says_what_it_did_not_change() {
        let refused = ImportOutcome::Refused {
            conflicts: vec![PreviewConflict { kind: "project_id".into(), id: "p".into(), detail: "exists".into() }],
        };
        let (tone, text) = import_headline(&refused);
        assert_eq!(tone, Tone::Error);
        assert!(text.contains("Nothing was changed"), "{text}");
        assert_eq!(import_detail(&refused).as_deref(), Some("project_id: exists"));

        let stale = ImportOutcome::ChecksumMismatch { message: "m".into() };
        assert_eq!(import_headline(&stale).0, Tone::Error);
        assert!(import_headline(&stale).1.contains("Preview the file again"));
    }

    #[test]
    fn an_unusable_file_and_a_failed_apply_each_say_nothing_changed() {
        let invalid = ImportOutcome::Invalid { message: "could not read x".into() };
        let (tone, text) = import_headline(&invalid);
        assert_eq!(tone, Tone::Error);
        assert!(text.contains("Nothing was changed"), "{text}");
        assert_eq!(import_detail(&invalid).as_deref(), Some("could not read x"));

        let failed = ImportOutcome::Failed { message: "board failed".into() };
        let (tone, text) = import_headline(&failed);
        assert_eq!(tone, Tone::Error);
        assert!(text.contains("nothing was kept"), "{text}");
        assert_eq!(import_detail(&failed).as_deref(), Some("board failed"));
    }

    #[test]
    fn an_incomplete_import_is_never_reported_as_reverted() {
        let incomplete = ImportOutcome::Incomplete { message: "revert failed".into() };
        let (tone, text) = import_headline(&incomplete);
        assert_eq!(tone, Tone::Warning);
        assert!(!text.to_lowercase().contains("reverted"), "{text}");
        assert!(text.contains("Restart SlashIt"), "{text}");
        assert_eq!(import_detail(&incomplete).as_deref(), Some("revert failed"));
    }

    #[test]
    fn an_applied_outcome_deserializes_from_the_backend_tag() {
        let json = serde_json::json!({
            "outcome": "applied",
            "project_id": "p",
            "project_name": "Alpha",
            "tasks": 2,
            "normalization": [],
            "repository_remote": null,
            "durable": false,
        });
        let parsed: ImportOutcome = serde_json::from_value(json).unwrap();
        assert!(matches!(parsed, ImportOutcome::Applied { durable: false, .. }));
    }
}
