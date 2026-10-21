//! A Project whose Task board cannot be read says so on its board, and takes no
//! Task changes until the file is repaired and SlashIt restarts.
//!
//! Two Projects each get a Task, so each has a board on disk. Between two
//! launches the second board is corrupted, the way a bad hand edit would leave
//! it. The journey then checks the warning, the inert board, the file's bytes,
//! the Project that is still healthy, a file edit made while the application
//! runs, and the restart that clears the warning once the file is repaired.
//! Every board is synthetic and lives in the journey's own state root.

use super::*;

const NOTICE: &str = "[data-testid=\"task-board-notice\"]";
const KANBAN: &str = "[data-testid=\"kanban-board\"]";

/// A board the parser refuses: an enum value with no variant. The value holds
/// the separator text a parser message uses, so a reason that trims at that
/// text would show its tail. The reason the application shows must name the
/// field, never this value.
const UNREADABLE_BOARD: &str =
    "version = 1\n\n[[tasks]]\nid = \"00000000-0000-4000-8000-000000000001\"\n\
title = \"Journey board\"\nstatus = \"Journey Secret, expected Journey Tail\"\n";

#[tokio::test(flavor = "multi_thread")]
async fn a_project_whose_task_board_cannot_be_read_says_so_and_takes_no_task_changes() {
    let context = TestContext::new("unreadable_board").expect("harness setup");
    let outcome = unreadable_board_journey(&context).await;
    context.finish(outcome);
}

async fn unreadable_board_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let healthy_repo = GitFixture::create(&root.join("healthy-repo"))?;
    let broken_repo = GitFixture::create(&root.join("broken-repo"))?;

    // Launch one: both boards are written with a Task each, then the app stops.
    let session = context.start_session("seed").await?;
    let outcome = seed_two_projects(session.driver(), &healthy_repo, &broken_repo).await;
    context.close_session(session, "seed", &outcome).await?;
    let seeded = outcome?;

    let board = board_file_holding(&root, &seeded.broken_title)?;
    let valid = std::fs::read(&board).with_context(|| format!("read {}", board.display()))?;
    std::fs::write(&board, UNREADABLE_BOARD)?;

    // Launch two: the warning, with the board still on disk and untouched.
    let session = context.start_session("unreadable").await?;
    let outcome = warning_on_launch(session.driver(), &seeded, &board).await;
    context
        .close_session(session, "unreadable", &outcome)
        .await?;
    outcome?;

    // Launch three, after the file is repaired: the restart clears the warning.
    std::fs::write(&board, &valid)?;
    let session = context.start_session("repaired").await?;
    let outcome = repaired_on_restart(session.driver(), &seeded).await;
    context.close_session(session, "repaired", &outcome).await?;
    outcome
}

struct Seeded {
    healthy_id: String,
    broken_id: String,
    broken_title: String,
}

async fn seed_two_projects(
    driver: &WebDriver,
    healthy_repo: &GitFixture,
    broken_repo: &GitFixture,
) -> Result<Seeded> {
    let healthy_id = create_project(driver, healthy_repo, "Healthy Journey Project").await?;
    let broken_id = create_project(driver, broken_repo, "Unreadable Journey Project").await?;
    super::task_runs::create_task(driver, &healthy_id, "Healthy journey task", &[]).await?;
    let (_, broken_title) =
        super::task_runs::create_task(driver, &broken_id, "Unreadable journey task", &[]).await?;
    Ok(Seeded {
        healthy_id,
        broken_id,
        broken_title,
    })
}

async fn create_project(driver: &WebDriver, repository: &GitFixture, name: &str) -> Result<String> {
    ui::assert_frontend_is_real(driver).await?;
    let repository_id = created_id(
        ui::invoke(
            driver,
            "create_repository",
            json!({ "localPath": repository.path(), "remoteUrl": Value::Null, "initialize": Value::Null }),
        )
        .await?,
        "create_repository",
    )?;
    created_id(
        ui::invoke(
            driver,
            "create_project",
            json!({ "name": name, "repositoryId": repository_id, "agentType": "claude_code" }),
        )
        .await?,
        "create_project",
    )
}

async fn warning_on_launch(driver: &WebDriver, seeded: &Seeded, board: &Path) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let corrupt = std::fs::read(board)?;

    // The Project that is not affected stays usable: its board renders, it has
    // no warning, and a Task can be created on it.
    select_project(driver, &seeded.healthy_id).await?;
    ui::visible(driver, KANBAN).await?;
    if count(driver, NOTICE).await? != 0 {
        bail!("the healthy Project shows an unreadable-board warning");
    }
    super::task_runs::create_task(driver, &seeded.healthy_id, "Still usable", &[]).await?;

    // The affected Project shows the warning on its own board.
    select_project(driver, &seeded.broken_id).await?;
    let notice = await_text(
        driver,
        NOTICE,
        |t| t.contains("Tasks could not be loaded for this Project"),
        "the warning on the affected Project",
    )
    .await?;
    check_notice_text(&notice, board)?;

    // The board is inert, so its Task controls cannot be reached.
    if !inert(driver, KANBAN).await? {
        bail!("the affected Project's board is not inert");
    }

    // Reading the warning changed nothing on disk.
    if std::fs::read(board)? != corrupt {
        bail!("showing the warning changed the unreadable board");
    }

    // A file edit while the app runs does not clear the warning: the report is
    // the startup load's record, not a fresh read of the file.
    let mut edited = corrupt.clone();
    edited.extend_from_slice(b"# edited while the application runs\n");
    std::fs::write(board, &edited)?;
    select_project(driver, &seeded.healthy_id).await?;
    select_project(driver, &seeded.broken_id).await?;
    await_text(
        driver,
        NOTICE,
        |t| t.contains("Tasks could not be loaded for this Project"),
        "the warning after the file was edited while running",
    )
    .await?;
    if std::fs::read(board)? != edited {
        bail!("the application changed the board while it ran");
    }
    Ok(())
}

async fn repaired_on_restart(driver: &WebDriver, seeded: &Seeded) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    select_project(driver, &seeded.broken_id).await?;
    ui::visible(driver, KANBAN).await?;
    if count(driver, NOTICE).await? != 0 {
        bail!("the warning is still shown after the repaired board was loaded");
    }
    if inert(driver, KANBAN).await? {
        bail!("the repaired Project's board is still inert");
    }
    await_text(
        driver,
        "[data-testid=\"task-title\"]",
        |t| t.contains(&seeded.broken_title),
        "the Task from the repaired board",
    )
    .await?;
    Ok(())
}

/// The reason is the technical part, and it must not carry the value from the
/// file. The guidance says the file was not overwritten and how to recover.
fn check_notice_text(notice: &str, board: &Path) -> Result<()> {
    for expected in [
        "has not been overwritten",
        "Restart SlashIt after correcting the file",
        "Technical reason",
    ] {
        if !notice.contains(expected) {
            bail!("the warning does not say {expected:?}: {notice:?}");
        }
    }
    if !notice.contains(&board.display().to_string()) {
        bail!(
            "the warning does not name the file {}: {notice:?}",
            board.display()
        );
    }
    for secret in ["Journey Secret", "Journey Tail"] {
        if notice.contains(secret) {
            bail!("the warning shows the value from the file: {notice:?}");
        }
    }
    Ok(())
}

async fn select_project(driver: &WebDriver, project_id: &str) -> Result<()> {
    ui::visible(
        driver,
        &format!("[data-testid=\"rail-project-{project_id}\"]"),
    )
    .await?
    .click()
    .await
    .with_context(|| format!("could not select project {project_id}"))?;
    ui::visible(driver, KANBAN).await.map(|_| ())
}

async fn count(driver: &WebDriver, selector: &str) -> Result<u64> {
    let value = page(
        driver,
        "return document.querySelectorAll(arguments[0]).length;",
        vec![json!(selector)],
    )
    .await?;
    value.as_u64().context("the element count is not a number")
}

async fn inert(driver: &WebDriver, selector: &str) -> Result<bool> {
    let value = page(
        driver,
        "const el = document.querySelector(arguments[0]); return !!el && el.closest('[inert]') !== null;",
        vec![json!(selector)],
    )
    .await?;
    Ok(value == json!(true))
}

/// The one board on disk that holds `title`: the Project's own file.
fn board_file_holding(root: &Path, title: &str) -> Result<PathBuf> {
    let mut holders = Vec::new();
    for file in toml_files(root)? {
        if std::fs::read_to_string(&file).is_ok_and(|text| text.contains(title)) {
            holders.push(file);
        }
    }
    match holders.as_slice() {
        [only] => Ok(only.clone()),
        other => bail!(
            "expected one board holding {title:?}, found {}",
            other.len()
        ),
    }
}
