// SPDX-License-Identifier: MPL-2.0
//! The PR-review apply scope of the unresolved-change record.
//!
//! One PR-review apply operation writes its record before its first fix agent
//! can edit the Task Checkout, and keeps it while the operation's successful
//! edits wait for their single consolidated commit. Only that operation's
//! items and its final commit may use the record. The record is cleared only
//! after that commit is made, or when the checkout is verified to hold no
//! uncommitted work. A failed item keeps the record with its reason, so later
//! items and every commit are refused until a person resolves the checkout.
//!
//! The record is the attempt-scoped record in `review_fix_residue`, with
//! `apply_scope` set and `attempt_id` holding the operation id. Commit-only
//! continuation (an operation that runs no agent) reuses a clean scope record
//! only when the caller proves the uncommitted edits belong to its plan, and
//! that the history since the record's base holds only SlashIt's own commits.

use uuid::Uuid;

use crate::domain::task::ReviewFixResidue;
use crate::review_fix_residue::{open_attempt, write_record, Ledger, RecordGate, RECORD_GATE};

/// Reason while an item's fix agent may still be editing the checkout, and
/// after a crash before an outcome was recorded.
pub const APPLY_STARTED_REASON: &str =
    "PR review fixes were started and their outcome is not recorded";

/// Reason when every item so far succeeded and their edits await the one commit.
pub const APPLY_CLEAN_REASON: &str =
    "PR review fixes applied and are waiting for their one commit";

impl Ledger {
    /// Records that item work of operation `op` may now edit the checkout. Creates
    /// the scope record when none exists; continues it only for the same
    /// operation whose previous item succeeded. Refused for any other record.
    pub async fn begin_apply_item(&self, task_id: Uuid, op: Uuid, working_dir: &str) -> Result<(), String> {
        let _gate = RECORD_GATE.lock().await;
        let (branch, existing) = {
            let tasks = self.tasks.read().await;
            let task = tasks.get(&task_id).ok_or_else(|| format!("task {task_id} is not on the board"))?;
            (task.branch_name.clone(), task.review_fix_residue.clone())
        };
        let branch = branch.ok_or_else(|| "this Task records no branch, so no fix was started".to_string())?;
        let head = crate::worktree::checkout_head_commit(working_dir)
            .await
            .map_err(|e| format!("the checkout's HEAD could not be read, so no fix was started: {e}"))?;
        let base_head = match existing {
            None => head,
            // Resumed: a later apply of the same Task, branch and checkout takes over a
            // clean scope record. Its edits are checked as the plan's own before each
            // launch, and the record is replaced in one write, so it is never absent.
            Some(r) if r.apply_scope && r.reason == APPLY_CLEAN_REASON && r.branch == branch && r.checkout_path == working_dir => {
                if !is_ancestor(&r.base_head, &head, working_dir).await {
                    return Err(format!(
                        "an earlier unresolved change on this Task is recorded ({}), but its base is not in the checkout's history, so no PR review fix was started",
                        r.reason
                    ));
                }
                r.base_head
            }
            Some(r) => {
                return Err(format!(
                    "an earlier unresolved change on this Task is recorded ({}), so no PR review fix was started",
                    r.reason
                ))
            }
        };
        write_record(
            &self.tasks,
            &self.storage,
            task_id,
            Some(ReviewFixResidue {
                attempt_id: op,
                branch,
                checkout_path: working_dir.to_string(),
                base_head,
                reason: APPLY_STARTED_REASON.to_string(),
                commit_authorized: false,
                apply_scope: true,
            }),
        )
        .await
    }

    /// Settles one item of operation `op`. `None` means its edits, if any, now
    /// await the consolidated commit; `Some(reason)` keeps the checkout unresolved.
    pub async fn settle_item(&self, task_id: Uuid, op: Uuid, keep: Option<String>) -> Result<(), String> {
        let _gate = RECORD_GATE.lock().await;
        let current = open_attempt(&self.tasks, task_id, op).await?;
        if !current.apply_scope {
            return Err("the unresolved record is not a PR review apply scope".to_string());
        }
        let reason = keep.unwrap_or_else(|| APPLY_CLEAN_REASON.to_string());
        write_record(
            &self.tasks,
            &self.storage,
            task_id,
            Some(ReviewFixResidue { reason, ..current }),
        )
        .await
    }

    /// Admits the consolidated commit of operation `op` (`None` for an operation
    /// that ran no item). Allowed only with no record, or with a clean apply
    /// scope that is `op`'s own, or any clean apply scope for commit-only
    /// continuation. The returned gate is held through the commit, and the
    /// record is returned for the provenance check and for clearing.
    pub async fn admit_apply_commit(
        &self,
        task_id: Uuid,
        op: Option<Uuid>,
    ) -> Result<(RecordGate, Option<ReviewFixResidue>), String> {
        let gate = RECORD_GATE.lock().await;
        let record = self.tasks.read().await.get(&task_id).and_then(|t| t.review_fix_residue.clone());
        match record {
            None => Ok((gate, None)),
            Some(r) if r.apply_scope && r.reason == APPLY_CLEAN_REASON && op.is_none_or(|o| o == r.attempt_id) => {
                Ok((gate, Some(r)))
            }
            Some(r) => Err(format!(
                "an unresolved change is recorded for this Task ({}), so nothing was committed",
                r.reason
            )),
        }
    }

    /// Clears the scope record `record` after its consolidated commit was made.
    /// The gate from [`Ledger::admit_apply_commit`] must still be held.
    pub async fn clear_apply(&self, _gate: &RecordGate, task_id: Uuid, record: &ReviewFixResidue) -> Result<(), String> {
        open_attempt(&self.tasks, task_id, record.attempt_id).await?;
        write_record(&self.tasks, &self.storage, task_id, None).await
    }

    /// Clears the scope record of `op` when it holds no uncommitted work, for an
    /// operation whose items made no change. Keeps it otherwise.
    pub async fn clear_apply_if_clean(&self, task_id: Uuid, op: Uuid) -> Result<(), String> {
        let _gate = RECORD_GATE.lock().await;
        let Ok(current) = open_attempt(&self.tasks, task_id, op).await else {
            return Ok(());
        };
        if !current.apply_scope || current.reason != APPLY_CLEAN_REASON {
            return Ok(());
        }
        if crate::worktree::checkout_has_work(&current.checkout_path, None).await? {
            return Ok(());
        }
        write_record(&self.tasks, &self.storage, task_id, None).await
    }
}

/// Whether `base` is an ancestor of `head` in `checkout`. Fails closed: an
/// unreadable answer counts as not an ancestor.
async fn is_ancestor(base: &str, head: &str, checkout: &str) -> bool {
    crate::host_process::tokio_command("git")
        .args(["merge-base", "--is-ancestor", base, head])
        .current_dir(checkout)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Proves that every commit on `checkout`'s HEAD since `base` is one of
/// `owned`, the commits SlashIt's own fix plan made. Commits SlashIt did not
/// make, such as an agent's own commit, make the proof fail, so a later commit
/// or push cannot publish them.
pub async fn history_since_is_owned(checkout: &str, base: &str, owned: &[String]) -> Result<(), String> {
    // A detached HEAD (a rebase in progress, or a checkout left detached) is refused
    // by the commit's own guard with its own diagnosis, so there is nothing to prove.
    let on_branch = crate::host_process::tokio_command("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(checkout)
        .output()
        .await
        .map_err(|e| format!("could not ask Git which branch the checkout is on: {e}"))?;
    if !on_branch.status.success() {
        return Ok(());
    }
    let output = crate::host_process::tokio_command("git")
        .args(["rev-list", &format!("{base}..HEAD")])
        .current_dir(checkout)
        .output()
        .await
        .map_err(|e| format!("could not list the commits on the Task branch: {e}"))?;
    if !output.status.success() {
        return Err("could not list the commits on the Task branch".to_string());
    }
    let listed = String::from_utf8_lossy(&output.stdout);
    for commit in listed.lines().map(str::trim).filter(|c| !c.is_empty()) {
        if !owned.iter().any(|o| o == commit) {
            return Err(format!(
                "commit {commit} on the Task branch was not made by a review fix, so nothing was committed or pushed over it"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::TaskStatus;
    use crate::review_fix_residue::resolve;
    use crate::worktree::CheckoutCommit;
    use std::path::{Path, PathBuf};

    struct Fx {
        _tmp: tempfile::TempDir,
        state: crate::AppState,
        ledger: Ledger,
        project_id: Uuid,
        task_id: Uuid,
        checkout: PathBuf,
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A Task on `task-x` in a fresh checkout with `a.txt` committed on `main`.
    async fn fixture() -> Fx {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = std::sync::Arc::new(crate::config::paths::AppPaths::with_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
            tmp.path().join("runtime"),
        ));
        let (state, _report) = crate::app_core::build_state_with_paths(paths).await.expect("build state");
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        git(&checkout, &["init", "-q", "-b", "main"]);
        git(&checkout, &["config", "user.name", "Test"]);
        git(&checkout, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(checkout.join("a.txt"), "a\n").unwrap();
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-q", "-m", "base"]);
        git(&checkout, &["checkout", "-q", "-b", "task-x"]);

        let project_id = Uuid::new_v4();
        let mut task = crate::test_helpers::create_test_task("pr apply task");
        task.project_id = project_id;
        task.branch_name = Some("task-x".to_string());
        task.worktree_path = Some(checkout.to_str().unwrap().to_string());
        task.status = TaskStatus::HumanReview;
        let task_id = task.id;
        state.task.tasks.write().await.insert(task_id, task);
        let ledger = Ledger::of(&state);
        Fx { _tmp: tmp, state, ledger, project_id, task_id, checkout }
    }

    fn dir(fx: &Fx) -> String {
        fx.checkout.to_str().unwrap().to_string()
    }

    async fn record(fx: &Fx) -> Option<ReviewFixResidue> {
        fx.state.task.tasks.read().await.get(&fx.task_id).and_then(|t| t.review_fix_residue.clone())
    }

    /// The record the next process would load from the durable board.
    fn reloaded(fx: &Fx) -> Option<ReviewFixResidue> {
        fx.state
            .storage
            .load_project_tasks(fx.project_id)
            .expect("load tasks")
            .into_iter()
            .find(|t| t.id == fx.task_id)
            .and_then(|t| t.review_fix_residue)
    }

    fn edit(fx: &Fx, name: &str) {
        std::fs::write(fx.checkout.join(name), format!("{name}\n")).unwrap();
    }

    fn commits(fx: &Fx) -> String {
        git(&fx.checkout, &["rev-list", "--count", "HEAD"])
    }

    #[tokio::test]
    async fn two_successful_items_share_one_scope_and_one_consolidated_commit() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        fx.ledger
            .begin_apply_item(fx.task_id, op, &dir(&fx))
            .await
            .expect("the same operation continues after a success");
        edit(&fx, "two.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();

        let (gate, scope) = fx.ledger.admit_apply_commit(fx.task_id, Some(op)).await.expect("the operation commits");
        let scope = scope.expect("its scope record");
        let outcome = crate::worktree::commit_checkout(&dir(&fx), "task-x", "task: fixes").await.unwrap();
        assert!(matches!(outcome, CheckoutCommit::Committed { .. }), "{outcome:?}");
        fx.ledger.clear_apply(&gate, fx.task_id, &scope).await.unwrap();
        drop(gate);

        assert!(record(&fx).await.is_none(), "cleared only after the commit");
        assert_eq!(commits(&fx), "2", "both items landed in one commit");
        assert!(fx.ledger.admit_commit(fx.task_id).await.is_ok());
    }

    #[tokio::test]
    async fn a_failed_item_after_a_success_keeps_its_edits_and_blocks_every_commit() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "partial.txt");
        fx.ledger.settle_item(fx.task_id, op, Some("the fix did not finish".into())).await.unwrap();

        assert!(fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.is_err(), "no later item runs");
        assert!(fx.ledger.admit_apply_commit(fx.task_id, Some(op)).await.is_err(), "no commit absorbs the partial edit");
        assert!(fx.ledger.admit_apply_commit(fx.task_id, None).await.is_err(), "nor commit-only continuation");
        assert_eq!(commits(&fx), "1", "no commit was made");
        assert!(fx.checkout.join("partial.txt").exists(), "the partial edit is preserved");
        assert_eq!(record(&fx).await.map(|r| r.reason), Some("the fix did not finish".to_string()));
    }

    #[tokio::test]
    async fn a_crash_between_items_keeps_the_scope_on_record_and_refuses_other_work() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        // The process stops here. The next process loads the scope record.
        let restarted = reloaded(&fx).expect("the scope survives the restart");
        assert!(restarted.apply_scope);
        assert_eq!(restarted.attempt_id, op);
        assert!(fx.ledger.admit_commit(fx.task_id).await.is_err(), "an ordinary Worker commit is refused");
        assert!(fx.ledger.begin_attempt(fx.task_id, &dir(&fx)).await.is_err(), "an AI Review fix is refused");
        assert!(
            resolve(&fx.ledger.authority, &fx.state.task.tasks, &fx.state.storage, fx.task_id, op, None)
                .await
                .is_err(),
            "uncommitted successful edits cannot be resolved away"
        );
    }

    /// A later operation on the same checkout resumes the clean scope, and the
    /// earlier one loses its commit authority. Until the resumed operation settles
    /// its own item, nothing may be committed under the record.
    #[tokio::test]
    async fn a_later_operation_resumes_the_scope_and_the_earlier_one_loses_its_commit() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();

        let other = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, other, &dir(&fx)).await.expect("resumed");
        assert!(fx.ledger.admit_apply_commit(fx.task_id, Some(op)).await.is_err(), "the superseded operation cannot commit");
        assert!(fx.ledger.admit_apply_commit(fx.task_id, Some(other)).await.is_err(), "not clean while its item is running");
        fx.ledger.settle_item(fx.task_id, other, None).await.unwrap();
        assert!(fx.ledger.admit_apply_commit(fx.task_id, Some(other)).await.is_ok(), "the resumed operation may commit");
    }

    #[tokio::test]
    async fn a_crash_before_the_final_commit_allows_only_plan_owned_continuation() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        // A later operation that runs no agent may commit the owed edits, and its
        // caller proves ownership. The record is what it commits under.
        let (gate, scope) = fx
            .ledger
            .admit_apply_commit(fx.task_id, None)
            .await
            .expect("commit-only continuation of a clean scope");
        assert_eq!(scope.map(|r| r.attempt_id), Some(op));
        drop(gate);
        assert!(reloaded(&fx).is_some(), "still protected until the commit is made");
    }

    #[tokio::test]
    async fn a_crash_after_the_final_commit_reconciles_without_approving_the_task() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        git(&fx.checkout, &["add", "-A"]);
        git(&fx.checkout, &["commit", "-q", "-m", "fixes"]);
        // The process stops after the commit and before the scope settles.
        assert!(reloaded(&fx).is_some());
        resolve(&fx.ledger.authority, &fx.state.task.tasks, &fx.state.storage, fx.task_id, op, None)
            .await
            .expect("a clean checkout on the Task branch reconciles");
        assert!(record(&fx).await.is_none());
        assert_eq!(
            fx.state.task.tasks.read().await.get(&fx.task_id).unwrap().status,
            TaskStatus::HumanReview,
            "reconciling is not approval"
        );
    }

    #[tokio::test]
    async fn an_agent_commit_under_the_fixes_is_refused_as_provenance() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        let base = git(&fx.checkout, &["rev-parse", "HEAD"]);
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        // An agent commits on the Task branch by itself, and the item's edit follows.
        edit(&fx, "agent.txt");
        git(&fx.checkout, &["add", "-A"]);
        git(&fx.checkout, &["commit", "-q", "-m", "agent commit"]);
        let agent = git(&fx.checkout, &["rev-parse", "HEAD"]);
        edit(&fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();

        let no_owned: Vec<String> = Vec::new();
        assert!(history_since_is_owned(&dir(&fx), &base, &no_owned).await.is_err(), "the agent commit is not ours");
        let plan_owned = vec![agent.clone()];
        assert!(history_since_is_owned(&dir(&fx), &base, &plan_owned).await.is_ok(), "a plan commit is ours");
    }

    #[tokio::test]
    async fn an_operation_without_uncommitted_work_clears_its_scope_only_when_clean() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(&fx)).await.unwrap();
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        edit(&fx, "left.txt");
        fx.ledger.clear_apply_if_clean(fx.task_id, op).await.unwrap();
        assert!(record(&fx).await.is_some(), "a dirty checkout keeps its scope");
        std::fs::remove_file(fx.checkout.join("left.txt")).unwrap();
        fx.ledger.clear_apply_if_clean(fx.task_id, op).await.unwrap();
        assert!(record(&fx).await.is_none(), "a clean checkout releases it");
    }

    /// One successful item of `op` leaves a clean scope record, as an apply does
    /// before its consolidated commit is refused or fails.
    async fn clean_scope_of(fx: &Fx, op: Uuid) -> ReviewFixResidue {
        fx.ledger.begin_apply_item(fx.task_id, op, &dir(fx)).await.unwrap();
        edit(fx, "one.txt");
        fx.ledger.settle_item(fx.task_id, op, None).await.unwrap();
        record(fx).await.expect("the clean scope record")
    }

    #[tokio::test]
    async fn a_later_apply_resumes_a_clean_scope_and_keeps_its_base() {
        let fx = fixture().await;
        let first = clean_scope_of(&fx, Uuid::new_v4()).await;
        let next = Uuid::new_v4();
        fx.ledger.begin_apply_item(fx.task_id, next, &dir(&fx)).await.expect("resumed");
        let resumed = record(&fx).await.expect("one record");
        assert_eq!(resumed.attempt_id, next, "the record now belongs to the new apply");
        assert_eq!(resumed.base_head, first.base_head, "the commit base is kept");
        assert!(resumed.apply_scope && resumed.reason == APPLY_STARTED_REASON);
    }

    #[tokio::test]
    async fn a_later_apply_is_refused_for_a_different_checkout_path() {
        let fx = fixture().await;
        let mut scope = clean_scope_of(&fx, Uuid::new_v4()).await;
        scope.checkout_path = "/elsewhere".to_string();
        write_record(&fx.ledger.tasks, &fx.ledger.storage, fx.task_id, Some(scope)).await.unwrap();
        assert!(fx.ledger.begin_apply_item(fx.task_id, Uuid::new_v4(), &dir(&fx)).await.is_err());
    }

    #[tokio::test]
    async fn a_later_apply_is_refused_when_the_recorded_base_is_not_in_history() {
        let fx = fixture().await;
        let mut scope = clean_scope_of(&fx, Uuid::new_v4()).await;
        scope.base_head = "0".repeat(40);
        write_record(&fx.ledger.tasks, &fx.ledger.storage, fx.task_id, Some(scope)).await.unwrap();
        let error = fx.ledger.begin_apply_item(fx.task_id, Uuid::new_v4(), &dir(&fx)).await.unwrap_err();
        assert!(error.contains("not in the checkout's history"), "{error}");
    }

    #[tokio::test]
    async fn a_later_apply_is_refused_for_a_failed_scope() {
        let fx = fixture().await;
        let op = Uuid::new_v4();
        clean_scope_of(&fx, op).await;
        fx.ledger.settle_item(fx.task_id, op, Some("an item failed".to_string())).await.unwrap();
        assert!(fx.ledger.begin_apply_item(fx.task_id, Uuid::new_v4(), &dir(&fx)).await.is_err());
    }
}
