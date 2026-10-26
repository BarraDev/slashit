// SPDX-License-Identifier: MPL-2.0
//! The Depends on control, used through the board.
//!
//! A Task is created through the Create Task form with its dependency chosen
//! from the Depends on control. The dependent Task then shows its parent in
//! its drawer, and the drawer's link opens the parent. When the dependent Task
//! runs, its branch starts from the parent's branch, which holds the parent's
//! own commit.
//!
//! The form is driven the way a person drives it. Creating the parent and
//! running either Task go through the same product commands as the other
//! journeys do. The dependency itself is never written through a command, so
//! what is checked here is what the control chose.

use super::human_review::{click, install_fakes};
use super::task_runs::{branch_name_of, git_is_ancestor, git_out, register_project, run_to_review};
use super::*;
use thirtyfour::components::SelectElement;

const NEW_TASK_BUTTON: &str = "[data-testid=\"new-task-button\"]";
const TITLE_INPUT: &str = "input[placeholder=\"Task title...\"]";
const DEPENDENCY_SELECT: &str = "[data-testid=\"task-dependency-select\"]";
const CREATE_BUTTON: &str = "//button[normalize-space(.)='Create Task']";
const DRAWER_TITLE: &str = "[data-testid=\"task-drawer-title\"]";
const DRAWER_DEPENDENCY: &str = "[data-testid=\"task-drawer-dependency\"]";
const CREATE_DEADLINE: Duration = Duration::from_secs(20);

#[tokio::test(flavor = "multi_thread")]
async fn a_task_created_with_the_depends_on_control_starts_from_its_parents_branch() {
    let context = TestContext::new("task_dependencies").expect("harness setup");
    let outcome = dependency_journey(&context).await;
    context.finish(outcome);
}

async fn dependency_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let (_agent, _gh) = install_fakes(context, &root)?;
    // Real work, so the parent's branch holds a commit the child must include.
    context.set_child_env(fake_agent::WRITE_OWN_FILE_VAR, "1");
    let repository = GitFixture::create(&root.join("fixture-repo"))?;

    let session = context.start_session("dependencies").await?;
    let outcome = depend_and_run(session.driver(), &repository).await;
    context.close_session(session, "dependencies", &outcome).await?;
    outcome
}

async fn depend_and_run(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    let project_id = register_project(driver, repository).await?;
    open_board(driver, &project_id).await?;

    let parent_title = unique_title("Parent");
    create_with_the_form(driver, &parent_title, None).await?;
    let parent = await_task_titled(driver, &project_id, &parent_title).await?;
    let parent_id = id_of(&parent)?;
    run_to_review(driver, &project_id, &parent_id, &parent_title).await?;
    let parent = await_task_titled(driver, &project_id, &parent_title).await?;
    let parent_branch = branch_name_of(&parent)?;

    let child_title = unique_title("Child");
    create_with_the_form(driver, &child_title, Some(&parent_title)).await?;
    let child = await_task_titled(driver, &project_id, &child_title).await?;
    let child_id = id_of(&child)?;
    if child.get("dependencies") != Some(&json!([parent_id])) {
        bail!("the child was created with dependencies {:?}, not its parent {parent_id}", child.get("dependencies"));
    }

    // The drawer names the parent, and its link opens the parent.
    open_drawer(driver, &child_id, &child_title).await?;
    let link = ui::visible(driver, DRAWER_DEPENDENCY)
        .await
        .context("the child's drawer shows no dependency")?;
    let shown = link.text().await?;
    if !shown.starts_with(&parent_title) {
        bail!("the drawer shows the dependency as {shown:?}, not {parent_title:?}");
    }
    link.click().await.context("could not press the dependency link")?;
    await_text(driver, DRAWER_TITLE, |title| title == parent_title, "the parent's drawer").await?;

    // The child's branch starts from the parent's branch, with the parent's commit in it.
    run_to_review(driver, &project_id, &child_id, &child_title).await?;
    let child = await_task_titled(driver, &project_id, &child_title).await?;
    let child_branch = branch_name_of(&child)?;
    let parent_tip = git_out(&repository.path, &["rev-parse", &parent_branch])?.trim().to_string();
    if child.get("base_commit").and_then(Value::as_str) != Some(parent_tip.as_str()) {
        bail!("the child starts at {:?}, not the parent's tip {parent_tip}", child.get("base_commit"));
    }
    if !child.get("branch_origin").is_some_and(|origin| origin.to_string().contains(&parent_branch)) {
        bail!("the child's branch origin {:?} does not name the parent's branch {parent_branch}", child.get("branch_origin"));
    }
    if !git_is_ancestor(&repository.path, &parent_branch, &child_branch)? {
        bail!("the child's branch {child_branch} does not include the parent's branch {parent_branch}");
    }
    Ok(())
}

/// Create a Task through the Create Task form. A dependency is chosen by its
/// parent's title, through the Depends on control.
async fn create_with_the_form(driver: &WebDriver, title: &str, depends_on: Option<&str>) -> Result<()> {
    click(driver, NEW_TASK_BUTTON, "the New Task button").await?;
    ui::visible(driver, TITLE_INPUT)
        .await
        .context("the Create Task form shows no title field")?
        .send_keys(title)
        .await?;
    if let Some(parent) = depends_on {
        let select = ui::visible(driver, DEPENDENCY_SELECT)
            .await
            .context("the Create Task form offers no Depends on control")?;
        SelectElement::new(&select)
            .await?
            .select_by_partial_text(parent)
            .await
            .with_context(|| format!("the Depends on control does not offer {parent}"))?;
    }
    driver
        .find(By::XPath(CREATE_BUTTON))
        .await
        .context("the Create Task form shows no Create Task button")?
        .click()
        .await?;
    Ok(())
}

/// A title no other journey or earlier run has used.
fn unique_title(prefix: &str) -> String {
    format!(
        "{prefix} {}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    )
}

/// The product's own record of the Task with this title, once it is listed.
async fn await_task_titled(driver: &WebDriver, project_id: &str, title: &str) -> Result<Value> {
    let started = Instant::now();
    loop {
        let listed = ui::invoke(driver, "list_tasks", json!({ "projectId": project_id })).await?;
        let found = listed
            .as_array()
            .and_then(|tasks| tasks.iter().find(|task| task.get("title").and_then(Value::as_str) == Some(title)));
        if let Some(task) = found {
            return Ok(task.clone());
        }
        if started.elapsed() > CREATE_DEADLINE {
            bail!("the product never listed a Task titled {title}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn id_of(task: &Value) -> Result<String> {
    task.get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("the listed Task has no identifier")
}
