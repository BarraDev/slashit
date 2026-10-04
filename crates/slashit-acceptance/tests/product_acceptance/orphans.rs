//! Settings > Storage lists the checkouts and task branches no task owns,
//! and reclaims each only on an explicit, confirmed request.
//!
//! The leftovers are made the way a failed cleanup would leave them: a
//! registered checkout under SlashIt's own worktree directory, and a task
//! branch, that no task records. Nothing on screen may remove either until
//! the person confirms, and a checkout with unsaved work is only explained.

use super::human_review::click;
use super::*;

const SETTINGS_STORAGE_TAB: &str = "[data-testid=\"settings-tab-storage\"]";
const SCAN: &str = "[data-testid=\"orphan-scan\"]";
const LIST: &str = "[data-testid=\"orphan-list\"]";
const NONE: &str = "[data-testid=\"orphan-none\"]";
const CHECKOUT_ROW: &str = "[data-testid=\"orphan-checkout\"]";
const BRANCH_ROW: &str = "[data-testid=\"orphan-branch\"]";
const RECLAIM: &str = "[data-testid=\"orphan-reclaim\"]";
const CONFIRM: &str = "[data-testid=\"orphan-confirm\"]";
const REFUSAL: &str = "[data-testid=\"orphan-refusal\"]";

#[tokio::test(flavor = "multi_thread")]
async fn leftover_checkouts_and_branches_are_listed_and_reclaimed_only_on_explicit_confirmation() {
    let context = TestContext::new("orphan_reclaim").expect("harness setup");
    let outcome = orphan_journey(&context).await;
    context.finish(outcome);
}

async fn orphan_journey(context: &TestContext) -> Result<()> {
    let root = context.state().path().to_path_buf();
    let repository = GitFixture::create(&root.join("fixture-repo"))?;
    let session = context.start_session("orphans").await?;
    let outcome = reclaim_leftovers(session.driver(), &repository).await;
    context.close_session(session, "orphans", &outcome).await?;
    outcome
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("could not run git {args:?}"))?;
    if !output.status.success() {
        bail!("git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// A branch name shaped like the ones SlashIt gives tasks.
fn task_branch(salt: u128) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
        + salt;
    format!("task-{:08x}-0000-4000-8000-{:012x}", (n >> 48) as u32, n & 0xffff_ffff_ffff)
}

async fn await_count(driver: &WebDriver, selector: &str, expected: usize, what: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let found = count(driver, selector).await?;
        if found == expected {
            return Ok(());
        }
        if started.elapsed() > RENDER_DEADLINE {
            bail!("{what}: {found} shown, expected {expected}");
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn reclaim_leftovers(driver: &WebDriver, repository: &GitFixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let Prerequisites { project_id, task_id, .. } =
        create_prerequisites(driver, repository, "Owned checkout", "Owns a checkout.").await?;
    // A task that owns a checkout and branch, which must never be listed.
    let owned = ui::invoke(driver, "create_worktree", json!({ "taskId": task_id })).await?;
    let owned = PathBuf::from(owned.as_str().context("create_worktree did not answer a path")?);
    let managed = owned.parent().context("the checkout has no parent directory")?.to_path_buf();

    // Leftovers: a dirty checkout, and a branch whose commits main has.
    let repo = repository.path_buf();
    let dirty_branch = task_branch(1);
    let dirty = managed.join(&dirty_branch);
    git(&repo, &["worktree", "add", "-q", "-b", &dirty_branch, &dirty.to_string_lossy()])?;
    std::fs::write(dirty.join("wip.txt"), "unsaved")?;
    let loose_branch = task_branch(2);
    git(&repo, &["branch", &loose_branch])?;

    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); \
         localStorage.setItem('slashit_selected_project', arguments[0]); return true;",
        vec![json!(project_id)],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;
    click(driver, SETTINGS_STORAGE_TAB, "the Storage settings").await?;

    // Nothing is scanned, listed or removed by opening the panel.
    await_count(driver, SCAN, 1, "the scan button").await?;
    await_count(driver, CHECKOUT_ROW, 0, "checkouts before scanning").await?;
    await_count(driver, BRANCH_ROW, 0, "branches before scanning").await?;

    click(driver, SCAN, "Scan for leftovers").await?;
    await_count(driver, CHECKOUT_ROW, 1, "orphan checkouts").await?;
    await_count(driver, BRANCH_ROW, 2, "orphan branches").await?;
    let listed = text_of(driver, LIST).await?.unwrap_or_default();
    if listed.contains(&owned.to_string_lossy().to_string()) {
        bail!("the task's own checkout is listed as a leftover: {listed}");
    }

    // The dirty checkout is explained, not offered; its branch cannot go
    // while it is checked out; the loose branch can.
    await_count(driver, &format!("{CHECKOUT_ROW} {REFUSAL}"), 1, "the dirty checkout's refusal").await?;
    let why = text_of(driver, &format!("{CHECKOUT_ROW} {REFUSAL}")).await?.unwrap_or_default();
    if !why.contains("uncommitted") {
        bail!("the dirty checkout's refusal does not say why: {why:?}");
    }
    await_count(driver, &format!("{CHECKOUT_ROW} {RECLAIM}"), 0, "a button on a dirty checkout").await?;
    await_count(driver, &format!("{BRANCH_ROW} {REFUSAL}"), 1, "the checked-out branch's refusal").await?;
    await_count(driver, &format!("{BRANCH_ROW} {RECLAIM}"), 1, "the one deletable branch").await?;

    // One click asks; nothing is deleted until it is confirmed.
    click(driver, &format!("{BRANCH_ROW} {RECLAIM}"), "Delete branch").await?;
    await_count(driver, CONFIRM, 1, "the confirmation").await?;
    if !repository.has_branch(&loose_branch)? {
        bail!("the branch was deleted before it was confirmed");
    }
    click(driver, CONFIRM, "Confirm").await?;
    await_count(driver, BRANCH_ROW, 1, "branches after the reclaim").await?;
    if repository.has_branch(&loose_branch)? {
        bail!("the confirmed branch is still in the repository");
    }

    // The person saves nothing and discards the file themselves; the next
    // scan then offers the checkout, and its branch only after that.
    std::fs::remove_file(dirty.join("wip.txt"))?;
    click(driver, SCAN, "Scan for leftovers").await?;
    await_count(driver, &format!("{CHECKOUT_ROW} {RECLAIM}"), 1, "the clean checkout's button").await?;
    click(driver, &format!("{CHECKOUT_ROW} {RECLAIM}"), "Remove checkout").await?;
    click(driver, CONFIRM, "Confirm").await?;
    await_count(driver, CHECKOUT_ROW, 0, "checkouts after the reclaim").await?;
    if dirty.exists() {
        bail!("the reclaimed checkout is still on disk");
    }
    if !repository.has_branch(&dirty_branch)? {
        bail!("removing the checkout deleted its branch");
    }
    if !owned.exists() {
        bail!("a task's own checkout was removed");
    }

    await_count(driver, &format!("{BRANCH_ROW} {RECLAIM}"), 1, "the branch, now deletable").await?;
    click(driver, &format!("{BRANCH_ROW} {RECLAIM}"), "Delete branch").await?;
    click(driver, CONFIRM, "Confirm").await?;
    await_count(driver, NONE, 1, "the empty state").await?;
    if repository.has_branch(&dirty_branch)? {
        bail!("the confirmed branch is still in the repository");
    }
    Ok(())
}
