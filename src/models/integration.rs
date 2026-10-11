//! Mirrors of the backend's local integration types
//! (`commands::integration`, `domain::integration`), and the words the
//! drawer uses for them.
//!
//! The backend decides what can be integrated and how; nothing here does.
//! What is here turns its answer into the lines a person reads before
//! confirming, so that those lines can be checked without a window.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStrategy {
    FastForward,
    MergeCommit,
    AlreadyContained,
}

impl IntegrationStrategy {
    /// The name of the strategy, as a person reads it.
    pub fn label(self) -> &'static str {
        match self {
            Self::FastForward => "Fast-forward",
            Self::MergeCommit => "Merge commit",
            Self::AlreadyContained => "Already contained",
        }
    }

    /// What the strategy does to the destination branch.
    pub fn effect(self) -> &'static str {
        match self {
            Self::FastForward => {
                "The branch moves straight to the approved commit. No commit is created and no history is rewritten."
            }
            Self::MergeCommit => {
                "The branch and the approved commit have diverged, so a merge commit with both as parents is created. Neither line is rewritten."
            }
            Self::AlreadyContained => "The branch already contains the approved commit, so nothing moves.",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Pending,
    Integrated,
}

/// Mirrors the backend `LocalIntegration`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalIntegration {
    pub state: IntegrationState,
    pub target_branch: String,
    pub target_old_oid: String,
    pub approved_tip: String,
    pub result_oid: String,
    pub strategy: IntegrationStrategy,
    #[serde(default)]
    pub checkout_path: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The integration was asked for as part of completing the Task.
    #[serde(default)]
    pub completes_task: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PreviewState {
    Ready,
    ReadyInCheckout,
    AlreadyContained,
    Integrated,
    /// The destination holds the approved work and the Task is not
    /// completed yet.
    CompletionPending,
    Pending,
    NeedsReconfirmation,
    Refused,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Reconfirm {
    pub branch_tip: String,
    pub commits: Vec<String>,
    /// What those commits change, as `<status> <path>`.
    #[serde(default)]
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RemoteTarget {
    pub name: String,
    pub tip: String,
    /// Where the local destination branch stands against it.
    #[serde(default)]
    pub relation: Option<RemoteRelation>,
}

/// Mirrors the backend `RemoteRelation`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRelation {
    InSync,
    LocalAhead,
    RemoteAhead,
    Diverged,
}

/// Mirrors the backend `IntegrationPreview`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct IntegrationPreview {
    pub state: PreviewState,
    #[serde(default)]
    pub target_branch: Option<String>,
    #[serde(default)]
    pub target_tip: Option<String>,
    #[serde(default)]
    pub approved_tip: Option<String>,
    #[serde(default)]
    pub strategy: Option<IntegrationStrategy>,
    #[serde(default)]
    pub commits: Vec<String>,
    #[serde(default)]
    pub unapproved_commits: u32,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub checked_out_at: Option<String>,
    #[serde(default)]
    pub checkout_problems: Vec<String>,
    #[serde(default)]
    pub has_remote: bool,
    #[serde(default)]
    pub remote_target: Option<RemoteTarget>,
    #[serde(default)]
    pub integration: Option<LocalIntegration>,
    #[serde(default)]
    pub observed: Option<String>,
    #[serde(default)]
    pub reconfirm: Option<Reconfirm>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// What a person confirmed: the target and approved commit that were shown,
/// and, when the target is checked out, that checkout.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IntegrationConfirmation {
    pub target_branch: String,
    pub target_tip: String,
    pub approved_tip: String,
    pub checkout_path: Option<String>,
}

/// The first twelve characters of an object id.
pub fn short(oid: &str) -> &str {
    oid.get(..12).unwrap_or(oid)
}

impl IntegrationPreview {
    /// Whether the drawer offers the integration at all.
    pub fn can_integrate(&self) -> bool {
        matches!(self.state, PreviewState::Ready | PreviewState::ReadyInCheckout | PreviewState::AlreadyContained)
    }

    /// The confirmation for exactly what this preview shows.
    pub fn confirmation(&self) -> Option<IntegrationConfirmation> {
        if !self.can_integrate() {
            return None;
        }
        Some(IntegrationConfirmation {
            target_branch: self.target_branch.clone()?,
            target_tip: self.target_tip.clone()?,
            approved_tip: self.approved_tip.clone()?,
            checkout_path: self.checked_out_at.clone(),
        })
    }

    /// The facts a person confirms, one line each, label first.
    pub fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = Vec::new();
        if let Some(branch) = &self.target_branch {
            facts.push(("Destination branch", branch.clone()));
        }
        if let Some(tip) = &self.approved_tip {
            facts.push(("Approved commit", short(tip).to_string()));
        }
        if let Some(tip) = &self.target_tip {
            facts.push(("Current destination commit", short(tip).to_string()));
        }
        if let Some(strategy) = self.strategy {
            facts.push(("Strategy", strategy.label().to_string()));
        }
        if let Some(path) = &self.checked_out_at {
            facts.push(("Checkout affected", path.clone()));
        }
        facts
    }

    /// What integrating does to the checkout that has the destination
    /// checked out, when one does.
    pub fn checkout_note(&self) -> Option<String> {
        let path = self.checked_out_at.as_ref()?;
        let branch = self.target_branch.as_deref().unwrap_or("The destination branch");
        Some(format!(
            "{branch} is checked out in {path}. Git will fast-forward that checkout, so its branch, \
             index and files all move to the new commit. It must have no uncommitted changes to \
             tracked files; any other file there is left alone."
        ))
    }

    /// Commits on the Task branch after the approved one, which are not
    /// approved and are not integrated.
    pub fn unapproved_note(&self) -> Option<String> {
        (self.unapproved_commits > 0).then(|| {
            let plural = if self.unapproved_commits == 1 { "commit" } else { "commits" };
            format!(
                "The Task branch has {} more {plural} after the approved commit. They were not \
                 approved and will not be integrated.",
                self.unapproved_commits
            )
        })
    }

    /// Where the remote stands, and that nothing is pushed.
    pub fn remote_note(&self) -> String {
        let branch = self.target_branch.as_deref().unwrap_or("the destination branch");
        if !self.has_remote {
            return "This repository has no remote named origin. Nothing will be pushed.".to_string();
        }
        match &self.remote_target {
            Some(remote) => {
                let standing = match remote.relation {
                    Some(RemoteRelation::RemoteAhead) => {
                        format!(" {} has commits {branch} does not.", remote.name)
                    }
                    Some(RemoteRelation::Diverged) => {
                        format!(" {} and {branch} have each moved on without the other.", remote.name)
                    }
                    _ => String::new(),
                };
                format!(
                    "This repository has a remote named origin ({} was at {} when last seen).{standing} \
                     Nothing will be pushed or fetched: {branch} changes only in this repository. \
                     Publish it later yourself, when you choose to. Unless you have \
                     chosen the local branch in Settings > Repository, new tasks keep starting from \
                     origin's default branch, which does not have this work.",
                    remote.name,
                    short(&remote.tip)
                )
            }
            None => format!(
                "This repository has a remote named origin. Nothing will be pushed or fetched: \
                 {branch} changes only in this repository. Publish it later yourself, when you \
                 choose to. Unless you have chosen the local branch in Settings > Repository, \
                 new tasks keep starting from origin's default branch, which does not have this \
                 work."
            ),
        }
    }

    /// What completing the task does besides moving the destination, one
    /// statement per line, for the confirmation of Complete and Integrate.
    pub fn completion_lines(task_checkout: Option<&str>, branch: Option<&str>) -> Vec<String> {
        let mut lines = vec![
            "The task is marked Done once the destination holds the approved commit. If it cannot be \
             completed, the integration stays recorded and the task stays in Human Review."
                .to_string(),
            "No pull request is created.".to_string(),
        ];
        if let Some(path) = task_checkout {
            lines.push(format!(
                "Its Task Checkout at {path} is removed. If Git refuses because it holds uncommitted \
                 changes, the checkout is kept."
            ));
        }
        match branch {
            Some(branch) => lines.push(format!(
                "The Task branch {branch} and all its commits are kept. SlashIt does not delete or push it."
            )),
            None => lines.push("This task has no branch recorded.".to_string()),
        }
        lines
    }

    /// What is true when the destination holds the work and the task is
    /// still in review.
    pub fn completion_pending_note(&self) -> String {
        let branch = self.target_branch.as_deref().unwrap_or("The destination branch");
        let mut note = format!(
            "{branch} already holds the approved work, and that is recorded. The task is not \
             completed yet. Finishing it repeats no integration."
        );
        if let Some(reason) = &self.reason {
            note.push(' ');
            note.push_str(reason);
        }
        note
    }

    /// What a pending integration needs, in a sentence, from what the
    /// destination branch shows.
    pub fn pending_note(&self) -> Option<String> {
        let record = self.integration.as_ref()?;
        let branch = &record.target_branch;
        Some(match self.observed.as_deref() {
            Some("applied") => format!(
                "{branch} already holds this work. Resume records that; nothing moves."
            ),
            Some("unmoved") => format!(
                "{branch} has not moved. Resume applies the plan below; Discard drops it and \
                 changes nothing."
            ),
            Some("moved") => format!(
                "{branch} moved somewhere else since this was planned. Resume drops the plan \
                 and changes nothing; review the integration again afterwards."
            ),
            Some("missing") => format!(
                "{branch} no longer exists. Resume drops the plan and changes nothing."
            ),
            _ => format!("This integration into {branch} was started and not finished."),
        })
    }

    /// Whether Discard is offered for a pending integration: once the branch
    /// holds the work, the backend refuses it.
    pub fn can_discard(&self) -> bool {
        self.observed.as_deref() != Some("applied")
    }
}

impl LocalIntegration {
    /// The one line that says what happened, for the drawer.
    pub fn result_line(&self) -> String {
        match self.strategy {
            IntegrationStrategy::AlreadyContained => format!(
                "{} already contained {}. Nothing moved.",
                self.target_branch,
                short(&self.approved_tip)
            ),
            strategy => format!(
                "Integrated into {} by {} at {}.",
                self.target_branch,
                strategy.label().to_lowercase(),
                short(&self.result_oid)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(c: char) -> String {
        c.to_string().repeat(40)
    }

    fn preview(state: PreviewState) -> IntegrationPreview {
        serde_json::from_value(serde_json::json!({
            "state": state_name(state),
            "target_branch": "main",
            "target_tip": oid('a'),
            "approved_tip": oid('b'),
            "strategy": "fast_forward",
            "commits": ["bbbbbbb add file"],
            "unapproved_commits": 0,
            "conflicts": [],
            "checked_out_at": null,
            "has_remote": false,
            "remote_target": null,
            "integration": null,
            "observed": null,
            "reconfirm": null,
            "reason": null,
        }))
        .expect("a preview as the backend sends it")
    }

    fn state_name(state: PreviewState) -> &'static str {
        match state {
            PreviewState::Ready => "ready",
            PreviewState::ReadyInCheckout => "ready_in_checkout",
            PreviewState::AlreadyContained => "already_contained",
            PreviewState::Integrated => "integrated",
            PreviewState::CompletionPending => "completion_pending",
            PreviewState::Pending => "pending",
            PreviewState::NeedsReconfirmation => "needs_reconfirmation",
            PreviewState::Refused => "refused",
        }
    }

    #[test]
    fn the_confirmation_is_exactly_what_the_preview_showed() {
        let mut p = preview(PreviewState::Ready);
        let c = p.confirmation().expect("ready can be confirmed");
        assert_eq!(c.target_branch, "main");
        assert_eq!(c.target_tip, oid('a'));
        assert_eq!(c.approved_tip, oid('b'));
        assert_eq!(c.checkout_path, None);

        // A checked-out target is confirmed by naming its checkout.
        p.state = PreviewState::ReadyInCheckout;
        p.checked_out_at = Some("/work/my repo".into());
        assert_eq!(p.confirmation().unwrap().checkout_path.as_deref(), Some("/work/my repo"));
    }

    #[test]
    fn completing_says_what_happens_to_the_checkout_the_branch_and_the_pull_request() {
        let lines = IntegrationPreview::completion_lines(Some("/data/worktrees/p/task-a"), Some("task-a")).join("\n");
        assert!(lines.contains("/data/worktrees/p/task-a is removed") || lines.contains("Task Checkout at /data/worktrees/p/task-a is removed"));
        assert!(lines.contains("uncommitted"), "{lines}");
        assert!(lines.contains("task-a and all its commits are kept"), "{lines}");
        assert!(lines.contains("No pull request is created."));
        assert!(lines.contains("stays in Human Review"), "a refused completion is described");

        let bare = IntegrationPreview::completion_lines(None, None).join("\n");
        assert!(!bare.contains("removed"), "no checkout, nothing removed: {bare}");
        assert!(bare.contains("no branch recorded"));
    }

    #[test]
    fn an_integration_waiting_for_its_completion_says_so_and_why() {
        let mut p = preview(PreviewState::CompletionPending);
        let plain = p.completion_pending_note();
        assert!(plain.contains("main already holds the approved work"), "{plain}");
        assert!(plain.contains("not completed yet") && plain.contains("repeats no integration"));
        p.reason = Some("The worktree at /w was kept: locked".into());
        assert!(p.completion_pending_note().ends_with("was kept: locked"));
    }

    #[test]
    fn a_record_written_before_completion_was_part_of_it_loads_as_not_completing() {
        let record: LocalIntegration = serde_json::from_value(serde_json::json!({
            "state": "integrated",
            "target_branch": "main",
            "target_old_oid": oid('a'),
            "approved_tip": oid('b'),
            "result_oid": oid('c'),
            "strategy": "fast_forward",
            "started_at": chrono::Utc::now(),
        }))
        .unwrap();
        assert!(!record.completes_task);
    }

    #[test]
    fn only_a_ready_preview_can_be_confirmed() {
        for state in [
            PreviewState::Integrated,
            PreviewState::CompletionPending,
            PreviewState::Pending,
            PreviewState::NeedsReconfirmation,
            PreviewState::Refused,
        ] {
            assert!(preview(state).confirmation().is_none(), "{state:?}");
            assert!(!preview(state).can_integrate(), "{state:?}");
        }
        for state in [PreviewState::Ready, PreviewState::ReadyInCheckout, PreviewState::AlreadyContained] {
            assert!(preview(state).can_integrate(), "{state:?}");
        }
    }

    #[test]
    fn the_facts_name_the_destination_the_commits_the_strategy_and_the_checkout() {
        let mut p = preview(PreviewState::ReadyInCheckout);
        p.checked_out_at = Some("/work/repo".into());
        p.strategy = Some(IntegrationStrategy::MergeCommit);
        let facts = p.facts();
        let get = |label: &str| facts.iter().find(|(l, _)| *l == label).map(|(_, v)| v.as_str());
        assert_eq!(get("Destination branch"), Some("main"));
        assert_eq!(get("Approved commit"), Some("bbbbbbbbbbbb"));
        assert_eq!(get("Current destination commit"), Some("aaaaaaaaaaaa"));
        assert_eq!(get("Strategy"), Some("Merge commit"));
        assert_eq!(get("Checkout affected"), Some("/work/repo"));
        assert!(p.checkout_note().unwrap().contains("/work/repo"));

        let plain = preview(PreviewState::Ready);
        assert!(plain.facts().iter().all(|(l, _)| *l != "Checkout affected"));
        assert!(plain.checkout_note().is_none());
    }

    #[test]
    fn unapproved_commits_are_named_and_never_called_integrated() {
        let mut p = preview(PreviewState::Ready);
        assert!(p.unapproved_note().is_none());
        p.unapproved_commits = 1;
        let note = p.unapproved_note().unwrap();
        assert!(note.contains("1 more commit ") && note.contains("will not be integrated"), "{note}");
        p.unapproved_commits = 3;
        assert!(p.unapproved_note().unwrap().contains("3 more commits"));
    }

    #[test]
    fn a_remote_is_mentioned_and_nothing_is_ever_said_to_be_pushed() {
        let mut p = preview(PreviewState::Ready);
        assert!(p.remote_note().contains("no remote named origin"));
        p.has_remote = true;
        let without_tip = p.remote_note();
        assert!(without_tip.contains("Nothing will be pushed"), "{without_tip}");
        p.remote_target = Some(RemoteTarget { name: "origin/main".into(), tip: oid('c'), relation: None });
        let with_tip = p.remote_note();
        assert!(with_tip.contains("origin/main") && with_tip.contains("cccccccccccc"), "{with_tip}");
        assert!(with_tip.contains("Nothing will be pushed"), "{with_tip}");
    }

    fn record(strategy: IntegrationStrategy, state: IntegrationState) -> LocalIntegration {
        LocalIntegration {
            state,
            target_branch: "main".into(),
            target_old_oid: oid('a'),
            approved_tip: oid('b'),
            result_oid: oid('c'),
            strategy,
            checkout_path: None,
            started_at: chrono::Utc::now(),
            completed_at: None,
            completes_task: false,
        }
    }

    #[test]
    fn the_result_names_the_destination_the_strategy_and_the_commit() {
        let line = record(IntegrationStrategy::MergeCommit, IntegrationState::Integrated).result_line();
        assert_eq!(line, "Integrated into main by merge commit at cccccccccccc.");
        let line = record(IntegrationStrategy::FastForward, IntegrationState::Integrated).result_line();
        assert_eq!(line, "Integrated into main by fast-forward at cccccccccccc.");
        let line = record(IntegrationStrategy::AlreadyContained, IntegrationState::Integrated).result_line();
        assert!(line.contains("already contained") && line.contains("Nothing moved"), "{line}");
    }

    #[test]
    fn a_pending_integration_says_what_resume_and_discard_will_do() {
        let mut p = preview(PreviewState::Pending);
        p.integration = Some(record(IntegrationStrategy::FastForward, IntegrationState::Pending));
        for (observed, resumes, discards) in [
            ("unmoved", "applies the plan", true),
            ("applied", "records that", false),
            ("moved", "drops the plan", true),
            ("missing", "drops the plan", true),
        ] {
            p.observed = Some(observed.to_string());
            let note = p.pending_note().unwrap();
            assert!(note.contains(resumes), "{observed}: {note}");
            assert_eq!(p.can_discard(), discards, "{observed}");
        }
    }

    #[test]
    fn a_preview_from_an_older_backend_still_loads() {
        let p: IntegrationPreview = serde_json::from_value(serde_json::json!({ "state": "refused", "reason": "no" })).unwrap();
        assert_eq!(p.state, PreviewState::Refused);
        assert!(!p.has_remote && p.checkout_problems.is_empty() && p.commits.is_empty());
    }
}
