// SPDX-License-Identifier: MPL-2.0
//! Portable Project export and import, end to end in two isolated instances.
//!
//! The export and import file dialogs are native windows that WebDriver cannot
//! drive, so this journey calls the commands those dialogs hand their paths to,
//! through the webview's own bridge. It then checks what the user sees: the
//! Import control opens its dialog, and the imported Project is listed after a
//! reload. Dialog cancellation and the uncertain-durability wording are covered
//! by the frontend state tests, not here.

use super::*;

const CHECKSUM_OF_NOTHING: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

async fn create_project(driver: &WebDriver, name: &str) -> Result<String> {
    created_id(ui::invoke(driver, "create_project", json!({
        "name": name, "repositoryId": Value::Null, "agentType": "claude_code"
    })).await?, "create_project")
}

/// The ids of every Project the instance lists.
async fn project_ids(driver: &WebDriver) -> Result<Vec<String>> {
    let listed = ui::invoke(driver, "list_projects", json!({})).await?;
    Ok(listed
        .as_array()
        .context("list_projects did not return a list")?
        .iter()
        .filter_map(|project| project.get("id").and_then(Value::as_str).map(str::to_string))
        .collect())
}

fn string_field<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("the reply has no `{field}`: {value}"))
}

/// The Import control is in the Project tab bar, or in the rail when the rail is on.
async fn import_control(driver: &WebDriver) -> Result<&'static str> {
    let in_tabs = "[data-testid=\"import-project-button\"]";
    let in_rail = "[data-testid=\"rail-import-project\"]";
    let present = page(
        driver,
        &format!("return !!document.querySelector('{in_tabs}') || !!document.querySelector('{in_rail}');"),
        vec![],
    )
    .await?;
    if present != Value::Bool(true) {
        bail!("no Import control is shown on the Project screen");
    }
    if page(driver, &format!("return !!document.querySelector('{in_tabs}');"), vec![]).await? == Value::Bool(true) {
        Ok(in_tabs)
    } else {
        Ok(in_rail)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_project_exports_then_imports_into_a_fresh_instance_as_an_inert_project() {
    // The exporting instance owns the Project and its Task.
    let source = TestContext::new("portable_export").expect("harness setup");
    let exported_file = source.state().path().join("portable-source.slashit-project.json");
    let file = exported_file.to_string_lossy().into_owned();

    let exported = async {
        let session = source.start_session("portable-export").await?;
        let driver = session.driver();
        let project = create_project(driver, "Portable Source").await?;
        ui::invoke(driver, "create_task", json!({"params": {
            "projectId": project,
            "title": "Portable task",
            "description": "Carried in the export and left in Backlog",
            "model": "sonnet",
            "planningMode": false,
            "dependencies": []
        }})).await?;
        ui::invoke(driver, "export_project", json!({"projectId": project, "path": file})).await?;
        if !exported_file.is_file() {
            bail!("the export did not create its file");
        }
        let written = std::fs::read(&exported_file).context("read the export")?;

        // Export never replaces an existing file.
        ui::invoke_expecting_refusal(driver, "export_project", json!({"projectId": project, "path": file})).await?;
        if std::fs::read(&exported_file).context("read the export again")? != written {
            bail!("a refused export changed the file it refused to replace");
        }

        source.close_session(session, "portable-export", &Ok::<(), anyhow::Error>(())).await?;
        Ok::<_, anyhow::Error>(project)
    }
    .await;
    let exported_project = match exported {
        Ok(project) => project,
        Err(error) => {
            source.finish::<()>(Err(error));
            return;
        }
    };

    // A fresh instance has no identity conflict with the export.
    let target = TestContext::new("portable_import").expect("harness setup");
    let outcome = async {
        let session = target.start_session("portable-import").await?;
        let driver = session.driver();
        ui::assert_frontend_is_real(driver).await?;
        if !project_ids(driver).await?.is_empty() {
            bail!("a fresh instance already lists Projects");
        }

        // Preview reads the file, reports it, and changes nothing.
        let preview = ui::invoke(driver, "preview_import", json!({"path": file})).await?;
        if preview.get("importable") != Some(&json!(true)) {
            bail!("the export should be importable into a fresh instance: {preview}");
        }
        if string_field(&preview["project"], "name")? != "Portable Source" {
            bail!("the preview names the wrong Project: {preview}");
        }
        if preview["counts"]["tasks"] != json!(1) {
            bail!("the preview counts the wrong number of Tasks: {preview}");
        }
        let checksum = string_field(&preview, "checksum")?.to_string();
        if !project_ids(driver).await?.is_empty() {
            bail!("previewing a file changed the Project list");
        }

        // Apply refuses a checksum other than the one the preview reported.
        let stale = ui::invoke(driver, "apply_import", json!({"path": file, "checksum": CHECKSUM_OF_NOTHING})).await?;
        if string_field(&stale, "outcome")? != "checksum_mismatch" {
            bail!("a checksum the user did not confirm was accepted: {stale}");
        }
        if !project_ids(driver).await?.is_empty() {
            bail!("a refused apply changed the Project list");
        }

        // The confirmed apply creates the Project as an inert record.
        let applied = ui::invoke(driver, "apply_import", json!({"path": file, "checksum": checksum})).await?;
        if string_field(&applied, "outcome")? != "applied" {
            bail!("the confirmed import did not apply: {applied}");
        }
        let imported = string_field(&applied, "project_id")?.to_string();
        if imported != exported_project {
            bail!("the import did not keep the exported Project id");
        }
        let listed = ui::invoke(driver, "list_projects", json!({})).await?;
        let project = listed
            .as_array()
            .and_then(|all| all.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(imported.as_str())))
            .context("the imported Project is not listed")?;
        if !project.get("repository_id").is_none_or(Value::is_null) {
            bail!("an imported Project was linked to a repository: {project}");
        }

        // The Task arrives in Backlog and nothing runs it.
        let tasks = ui::invoke(driver, "list_tasks", json!({"projectId": imported})).await?;
        let task = tasks.as_array().and_then(|all| all.first()).context("the imported Task is missing")?;
        if string_field(task, "status")? != "backlog" {
            bail!("an imported Task is not in Backlog: {task}");
        }

        // A second import of the same file is refused by identity and changes nothing.
        let again = ui::invoke(driver, "apply_import", json!({"path": file, "checksum": checksum})).await?;
        if string_field(&again, "outcome")? != "refused" {
            bail!("a duplicate import was not refused: {again}");
        }
        if project_ids(driver).await?.len() != 1 {
            bail!("a refused import changed the Project list");
        }

        // The imported Project survives a reload, and the Import control opens its dialog.
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;
        if !project_ids(driver).await?.contains(&imported) {
            bail!("the imported Project did not survive a reload");
        }
        let control = import_control(driver).await?;
        ui::visible(driver, control).await?.click().await.context("could not open the Import dialog")?;
        ui::visible(driver, "[data-testid=\"import-project-modal\"]").await?;
        ui::visible(driver, "[data-testid=\"import-choose-file\"]").await?;
        page(driver, "document.querySelector('[data-testid=\"import-close\"]').click(); return true;", vec![]).await?;

        target.close_session(session, "portable-import", &Ok::<(), anyhow::Error>(())).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    source.finish::<()>(Ok(()));
    target.finish(outcome);
}
