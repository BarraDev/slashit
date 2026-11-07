// SPDX-License-Identifier: MPL-2.0
//! Portable Project export and import, end to end in two isolated instances.
//!
//! The import is driven through the rendered Import dialog: the Import control
//! opens it, Choose file selects a fixture, and the journey clicks Preview's
//! Close, Import Project and the refusal paths itself. The one replacement is
//! the native file dialog. A debug build reads its answer from the file that
//! `SLASHIT_DEBUG_IMPORT_CHOICE_FILE` names (an empty file is a cancelled dialog;
//! see `test_import_choice` in the desktop command module). Release builds never
//! read it. Export is still started through its command, because its save dialog
//! is outside this journey.
//!
//! Not covered here: the native dialog windows themselves, the uncertain-durability
//! headline (the frontend unit tests check its wording), and a power loss between
//! the commit and the record removal (the backend startup tests simulate that).
//! Worker inactivity is checked against the fake agent's invocation log, which
//! records every run the agent serves. No timing is used to prove absence.

use super::*;

/// Read by the debug build of `choose_import_file`. Keep the two in step.
const IMPORT_CHOICE_VAR: &str = "SLASHIT_DEBUG_IMPORT_CHOICE_FILE";

/// The export file the backend tests use: one Project, and a Task in Human Review
/// that holds a current approval. Importing it must withdraw the approval and
/// return the Task to Backlog. The file is read at compile time from the backend's testdata.
const GOLDEN_BUNDLE: &[u8] =
    include_bytes!("../../../../src-tauri/src/portability/testdata/export-v1.golden.json");
const GOLDEN_PROJECT_ID: &str = "00000000-0000-0000-0000-000000000001";
const GOLDEN_NAME: &str = "Fixture";

const SOURCE_NAME: &str = "Portable Source";
const CHECKSUM_OF_NOTHING: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

const PREVIEW: &str = "[data-testid=\"import-preview\"]";
const CONFIRM: &str = "[data-testid=\"import-confirm\"]";
const CONFLICTS: &str = "[data-testid=\"import-conflicts\"]";
const STATUS: &str = "[data-testid=\"import-status\"]";
const HEADLINE: &str = "[data-testid=\"import-headline\"]";
const DIALOG: &str = "[data-testid=\"import-project-modal\"]";
const CLOSE: &str = "[data-testid=\"import-close\"]";
const CHOOSE: &str = "[data-testid=\"import-choose-file\"]";

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

async fn is_present(driver: &WebDriver, selector: &str) -> Result<bool> {
    Ok(page(driver, &format!("return !!document.querySelector('{selector}');"), vec![]).await?
        == Value::Bool(true))
}

/// Text of the first element that matches `selector`, if there is one.
async fn text_of(driver: &WebDriver, selector: &str) -> Result<String> {
    let text = page(
        driver,
        &format!("return document.querySelector('{selector}')?.textContent?.trim() ?? '';"),
        vec![],
    )
    .await?;
    Ok(text.as_str().unwrap_or_default().to_string())
}

/// Wait until the first element matching `selector` shows text containing `needle`,
/// and return that text. The panel is re-rendered as its state changes, so the text
/// is read again on each poll rather than through a held element handle.
async fn wait_for_text(driver: &WebDriver, selector: &str, needle: &str) -> Result<String> {
    let deadline = Instant::now() + ui::ELEMENT_TIMEOUT;
    loop {
        let text = text_of(driver, selector).await?;
        if text.contains(needle) {
            return Ok(text);
        }
        if Instant::now() >= deadline {
            bail!("{selector} does not show {needle:?} within {}s; it shows {text:?}", ui::ELEMENT_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until `selector` leaves the document. A dialog the user closes is removed
/// at once; the bound only stops a dialog that stays open from hanging the run.
async fn wait_until_gone(driver: &WebDriver, selector: &str) -> Result<()> {
    let deadline = Instant::now() + ui::ELEMENT_TIMEOUT;
    while is_present(driver, selector).await? {
        if Instant::now() >= deadline {
            bail!("{selector} is still shown after {}s", ui::ELEMENT_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

async fn open_import(driver: &WebDriver) -> Result<()> {
    let control = import_control(driver).await?;
    ui::visible(driver, control).await?.click().await.context("could not click the Import control")?;
    if ui::visible(driver, DIALOG).await.is_err() {
        // Name what sits under the control's centre, so an overlay is visible in the failure.
        let probe = page(
            driver,
            &format!(
                "const c = document.querySelector('{control}'); const r = c.getBoundingClientRect(); \
                 const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); \
                 return {{ rect: [r.x, r.y, r.width, r.height], topmost: top ? top.outerHTML.slice(0, 200) : null }};"
            ),
            vec![],
        )
        .await?;
        bail!("the Import dialog did not open after clicking {control}: {probe}");
    }
    Ok(())
}

/// Click Close. This is also the cancel for a file that was only previewed.
async fn close_import(driver: &WebDriver) -> Result<()> {
    let close = ui::visible(driver, CLOSE).await?;
    close.scroll_into_view().await.context("could not scroll to Close")?;
    close.click().await.context("could not click Close")?;
    wait_until_gone(driver, DIALOG).await
}

/// Set what the dialog answers (a path, or an empty line for a cancel), then click Choose file.
async fn choose(driver: &WebDriver, answer_file: &Path, answer: &str) -> Result<()> {
    std::fs::write(answer_file, answer).context("could not write the dialog's answer")?;
    ui::visible(driver, CHOOSE).await?.click().await.context("could not click Choose file")?;
    Ok(())
}

/// The fake agent records every invocation. A bare `claude --version` probe is the
/// application's own readiness check, and it is the only launch not counted here.
/// Every other launch counts, prompted or not. The check is point-in-time, after the
/// step that could have started a run, and the journey does not wait for a later start.
fn assert_no_worker(agent: &FakeAgent, when: &str) -> Result<()> {
    let runs = agent
        .invocations()?
        .iter()
        .filter(|run| !(run.args.len() == 1 && run.args[0].as_os_str() == OsStr::new("--version")))
        .count();
    if runs != 0 {
        bail!("the import started {runs} agent run(s) {when}");
    }
    Ok(())
}

/// Click Import Project. The button sits at the foot of a dialog that scrolls, so
/// it is brought into view first, as a user scrolls to it.
async fn confirm_import(driver: &WebDriver, what: &str) -> Result<()> {
    let button = ui::visible(driver, CONFIRM).await?;
    button.scroll_into_view().await.context("could not scroll to Import Project")?;
    button.click().await.with_context(|| format!("could not confirm {what}"))?;
    Ok(())
}

/// Wait until the Project tab bar or the rail shows `name`. The tab is added when the
/// import's Project lookup returns, so the first read can precede the re-render.
/// The bound fails the run with what the navigation actually shows.
async fn wait_for_navigation(driver: &WebDriver, project_id: &str, name: &str) -> Result<()> {
    let deadline = Instant::now() + ui::ELEMENT_TIMEOUT;
    loop {
        // The tab bar shows names. The rail shows an initial, and its item carries the name as its title.
        let shown = page(
            driver,
            &format!(
                "const tab = document.querySelector('[data-testid=\"project-tabs\"]')?.textContent || ''; \
                 const rail = document.querySelector('[data-testid=\"rail-project-{project_id}\"]')?.getAttribute('title') || ''; \
                 return tab + ' | ' + rail;"
            ),
            vec![],
        )
        .await?;
        let shown = shown.as_str().unwrap_or_default().to_string();
        if shown.contains(name) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the imported Project is not in the navigation: {shown:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn task_statuses(driver: &WebDriver, project_id: &str) -> Result<Vec<(String, String)>> {
    let tasks = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
    tasks
        .as_array()
        .context("list_tasks did not return a list")?
        .iter()
        .map(|task| -> Result<(String, String)> {
            Ok((string_field(task, "id")?.to_string(), string_field(task, "status")?.to_string()))
        })
        .collect()
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
        let project = create_project(driver, SOURCE_NAME).await?;
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

    // A fresh instance has no identity conflict with the export. Its agent is the
    // fake one on PATH, so any run the import starts is recorded.
    let target = TestContext::new("portable_import").expect("harness setup");
    let agent = FakeAgent::install(target.state().path()).expect("install the fake agent");
    target.set_child_env("PATH", agent.path_value());
    target.set_child_env(fake_agent::MARKER_DIR_VAR, agent.marker_dir());
    let answer = target.state().path().join("import-dialog-answer.txt");
    target.set_child_env(IMPORT_CHOICE_VAR, &answer);
    std::fs::write(&answer, "").expect("create the dialog answer");

    let outcome = async {
        let session = target.start_session("portable-import").await?;
        let driver = session.driver();
        let result = import_through_the_dialog(driver, &target, &agent, &answer, &file, &exported_project).await;
        let closed = target.close_session(session, "portable-import", &result).await;
        result.and(closed.map(|_| ()))
    }
    .await;
    source.finish::<()>(Ok(()));
    target.finish(outcome);
}

/// The import journey in the rendered dialog. Each block names the behavior it proves.
async fn import_through_the_dialog(
    driver: &WebDriver,
    target: &TestContext,
    agent: &FakeAgent,
    answer: &Path,
    file: &str,
    exported_project: &str,
) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    if !project_ids(driver).await?.is_empty() {
        bail!("a fresh instance already lists Projects");
    }

    // A cancelled dialog changes nothing: no preview appears, and the Choose control stays.
    open_import(driver).await?;
    choose(driver, answer, "").await?;
    if is_present(driver, PREVIEW).await? {
        bail!("a cancelled dialog produced a preview");
    }
    ui::visible(driver, CHOOSE).await?;
    if !project_ids(driver).await?.is_empty() {
        bail!("a cancelled dialog changed the Project list");
    }

    // Preview shows what the file holds and creates no Project.
    choose(driver, answer, file).await?;
    ui::visible(driver, PREVIEW).await?;
    if text_of(driver, "[data-testid=\"import-project-name\"]").await? != SOURCE_NAME {
        bail!("the preview names the wrong Project");
    }
    if text_of(driver, "[data-testid=\"import-task-count\"]").await? != "1" {
        bail!("the preview counts the wrong number of Tasks");
    }
    if !is_present(driver, CONFIRM).await? {
        bail!("an importable file offers no Import Project control");
    }
    if !project_ids(driver).await?.is_empty() {
        bail!("previewing a file created a Project");
    }
    assert_no_worker(agent, "during the preview")?;

    // Close is the cancel: the previewed file is not imported.
    close_import(driver).await?;
    if !project_ids(driver).await?.is_empty() {
        bail!("closing the preview created a Project");
    }

    // Import Project applies the file through the visible control, and only then.
    open_import(driver).await?;
    choose(driver, answer, file).await?;
    ui::visible(driver, PREVIEW).await?;
    confirm_import(driver, "the Portable Source file").await?;
    ui::visible(driver, HEADLINE).await?;
    if text_of(driver, HEADLINE).await? != format!("Imported \"{SOURCE_NAME}\".") {
        bail!("the import was not reported as a confirmed success");
    }
    let tone = page(driver, &format!("return document.querySelector('{HEADLINE}')?.className ?? '';"), vec![]).await?;
    if !tone.as_str().unwrap_or_default().contains("text-emerald-300") {
        bail!("the confirmed import is not shown in the success tone: {tone}");
    }
    close_import(driver).await?;

    // The Project is listed and appears in the navigation.
    let listed = project_ids(driver).await?;
    if listed != vec![exported_project.to_string()] {
        bail!("the import did not add exactly the exported Project: {listed:?}");
    }
    wait_for_navigation(driver, exported_project, SOURCE_NAME).await?;
    let imported = ui::invoke(driver, "list_projects", json!({})).await?;
    let project = imported
        .as_array()
        .and_then(|all| all.iter().find(|p| p.get("id").and_then(Value::as_str) == Some(exported_project)))
        .context("the imported Project is not listed")?;
    if !project.get("repository_id").is_none_or(Value::is_null) {
        bail!("an imported Project was linked to a repository: {project}");
    }

    // The Task arrives in Backlog, and no Worker started on the way.
    let tasks = task_statuses(driver, exported_project).await?;
    if tasks.len() != 1 || tasks[0].1 != "backlog" {
        bail!("the imported Tasks are not inert in Backlog: {tasks:?}");
    }
    assert_no_worker(agent, "during the import")?;

    // A file with a Task in Human Review and a current approval. Import withdraws the
    // approval, moves the Task to Backlog, and says so before the user confirms.
    let golden = target.state().path().join("golden-export.slashit-project.json");
    std::fs::write(&golden, GOLDEN_BUNDLE).context("write the approval fixture")?;
    let golden_file = golden.to_string_lossy().into_owned();
    open_import(driver).await?;
    choose(driver, answer, &golden_file).await?;
    ui::visible(driver, PREVIEW).await?;
    let changes = text_of(driver, "[data-testid=\"import-normalization\"]").await?;
    if !changes.contains("Approvals are withdrawn") || !changes.contains("return to Backlog") {
        bail!("the preview does not say what the import changes: {changes:?}");
    }
    assert_no_worker(agent, "while the approval fixture was previewed")?;
    confirm_import(driver, "the approval fixture").await?;
    ui::visible(driver, HEADLINE).await?;
    if text_of(driver, HEADLINE).await? != format!("Imported \"{GOLDEN_NAME}\".") {
        bail!("the approval fixture was not imported as a confirmed success");
    }
    close_import(driver).await?;
    let golden_tasks = task_statuses(driver, GOLDEN_PROJECT_ID).await?;
    if golden_tasks.len() != 2 || golden_tasks.iter().any(|(_, status)| status != "backlog") {
        bail!("a historical Human Review approval kept its Task out of Backlog: {golden_tasks:?}");
    }
    assert_no_worker(agent, "after the approval fixture was imported")?;

    // The same file again: the identity conflict is shown, and nothing can be confirmed.
    open_import(driver).await?;
    choose(driver, answer, file).await?;
    ui::visible(driver, CONFLICTS).await?;
    if is_present(driver, CONFIRM).await? {
        bail!("a file that conflicts with this instance can be confirmed");
    }
    close_import(driver).await?;
    // The backend refuses the same apply without the dialog as well, and refuses a
    // checksum the user did not preview.
    let preview = ui::invoke(driver, "preview_import", json!({"path": file})).await?;
    let stale = ui::invoke(driver, "apply_import", json!({"path": file, "checksum": CHECKSUM_OF_NOTHING})).await?;
    if string_field(&stale, "outcome")? != "checksum_mismatch" {
        bail!("an unconfirmed checksum was accepted: {stale}");
    }
    let refused = ui::invoke(driver, "apply_import", json!({
        "path": file,
        "checksum": string_field(&preview, "checksum")?
    }))
    .await?;
    if string_field(&refused, "outcome")? != "refused" {
        bail!("a duplicate import was not refused: {refused}");
    }

    // A file that is not an export is shown as an error, not as a success, and nothing is confirmable.
    let garbage = target.state().path().join("not-an-export.slashit-project.json");
    std::fs::write(&garbage, "this is not a SlashIt export").context("write the invalid file")?;
    open_import(driver).await?;
    choose(driver, answer, &garbage.to_string_lossy()).await?;
    let status = wait_for_text(driver, STATUS, "cannot be imported").await?;
    if !status.contains("cannot be imported") || !status.contains("Nothing was changed.") {
        bail!("an invalid file was not shown as a refusal: {status:?}");
    }
    if is_present(driver, CONFIRM).await? {
        bail!("an invalid file can be confirmed");
    }
    close_import(driver).await?;

    // Everything survives a reload, and nothing ran.
    driver.refresh().await.context("could not reload the application window")?;
    ui::assert_frontend_is_real(driver).await?;
    let mut listed = project_ids(driver).await?;
    listed.sort();
    let mut expected = vec![exported_project.to_string(), GOLDEN_PROJECT_ID.to_string()];
    expected.sort();
    if listed != expected {
        bail!("the imported Projects did not survive a reload: {listed:?}");
    }
    let after_reload = task_statuses(driver, exported_project).await?;
    if after_reload.iter().any(|(_, status)| status != "backlog") {
        bail!("an imported Task left Backlog after a reload: {after_reload:?}");
    }
    assert_no_worker(agent, "after the reload")?;

    // The Import control still opens the dialog after a reload.
    open_import(driver).await?;
    close_import(driver).await?;
    Ok(())
}
