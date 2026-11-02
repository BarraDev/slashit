// SPDX-License-Identifier: MPL-2.0
//! The decisions behind the pull request recovery panel, free of signals and
//! services so the host can test each rule.

use crate::models::{
    ResolveUnconfirmedOutcome, Task, UnconfirmedOutcome, UnconfirmedPrCreate,
};

/// Where a task's request to open its pull request stands, as far as the
/// person is concerned. Three states that must never read alike.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryState {
    Unknown(UnconfirmedPrCreate),
    Verified { url: String },
    ResolvedByPerson(UnconfirmedPrCreate),
}

/// The state to show, or `None` when there is nothing about an unanswered
/// request to say.
///
/// An unanswered request is `Unknown` whatever else is recorded. Without one,
/// the answer to a link is `Verified` only while the task holds that very link
/// (the backend linked it after checking it is the branch's own). Otherwise the
/// latest decision of the person to discard a request is `ResolvedByPerson`,
/// for as long as the task has no pull request: it is the person's word, and
/// stays distinct from anything GitHub confirmed. A pull request linked later
/// is the task's pull request like any other.
pub fn recovery_state(task: &Task, last: Option<&UnconfirmedOutcome>) -> Option<RecoveryState> {
    if let Some(attempt) = &task.pr_create_unconfirmed {
        return Some(RecoveryState::Unknown(attempt.clone()));
    }
    if let Some(UnconfirmedOutcome::Linked { url }) = last {
        if task.pr_url.as_deref() == Some(url.as_str()) {
            return Some(RecoveryState::Verified { url: url.clone() });
        }
    }
    let linked = task.pr_url.is_some() || task.external_refs.iter().any(|r| r.is_pr());
    if linked {
        return None;
    }
    task.activity
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            slashit_activity::Kind::PrCreateUnconfirmedDiscarded { branch, commit, started_at } => {
                Some(RecoveryState::ResolvedByPerson(UnconfirmedPrCreate {
                    branch: branch.clone(),
                    commit: commit.clone(),
                    started_at: *started_at,
                }))
            }
            _ => None,
        })
}

/// Whether the confirmation opened for `shown` still describes the task's
/// record. Any difference, or no record, means the person has not confirmed
/// what the task holds now.
pub fn still_current(shown: &UnconfirmedPrCreate, live: Option<&UnconfirmedPrCreate>) -> bool {
    live == Some(shown)
}

/// Whether the discard may be sent: the person acknowledged the risk, and no
/// request of this panel is in flight.
pub fn can_discard(acknowledged: bool, busy: bool) -> bool {
    acknowledged && !busy
}

/// A cheap shape check before a URL is offered to the backend, which decides.
pub fn looks_like_pull_request_url(url: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        return false;
    };
    matches!(
        rest.split('/').collect::<Vec<_>>().as_slice(),
        [host, owner, repo, "pull", number]
            if !host.is_empty() && !owner.is_empty() && !repo.is_empty()
                && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Warn,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub tone: Tone,
    pub text: String,
}

/// What an answer says, in words that keep a person's decision apart from
/// something GitHub confirmed.
pub fn outcome_note(outcome: &UnconfirmedOutcome) -> Note {
    match outcome {
        UnconfirmedOutcome::Linked { url } => Note {
            tone: Tone::Good,
            text: format!(
                "Linked {url}. SlashIt checked that it is this branch's own pull request, in this \
                 repository, at the work the request was made for."
            ),
        },
        UnconfirmedOutcome::Discarded { durable: true } => Note {
            tone: Tone::Info,
            text: "You confirmed that no pull request exists, and the unanswered request was \
                   discarded. This was your decision; GitHub did not confirm it. Nothing was sent \
                   to GitHub. Creating the pull request again goes through the usual checks and \
                   approval."
                .to_string(),
        },
        UnconfirmedOutcome::Discarded { durable: false } => Note {
            tone: Tone::Warn,
            text: "You confirmed that no pull request exists, and the unanswered request was \
                   discarded, but SlashIt could not confirm that the change reached stable storage. \
                   It may come back if the computer stops before the next save. Nothing was sent to \
                   GitHub."
                .to_string(),
        },
        UnconfirmedOutcome::PullRequestFound { url } => Note {
            tone: Tone::Warn,
            text: format!(
                "GitHub lists a pull request for this branch: {url}. Nothing was discarded. Link it \
                 instead, so a second one is not created."
            ),
        },
        UnconfirmedOutcome::NotThisPullRequest { reason } => Note {
            tone: Tone::Warn,
            text: format!("That pull request was not linked: {reason}"),
        },
        UnconfirmedOutcome::Stale { reason } => Note { tone: Tone::Warn, text: reason.clone() },
    }
}

/// The pull request a "found" answer offers to link.
pub fn found_url(outcome: &UnconfirmedOutcome) -> Option<&str> {
    match outcome {
        UnconfirmedOutcome::PullRequestFound { url } => Some(url),
        _ => None,
    }
}

/// The task an answer carries, to be applied to the board as its new record.
pub fn answer_task(answer: &ResolveUnconfirmedOutcome) -> Option<&Task> {
    answer.task.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TaskStatus, UnconfirmedResolution};
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    const COMMIT: &str = "1111111111111111111111111111111111111111";
    const URL: &str = "https://github.com/o/r/pull/41";

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).expect("valid timestamp")
    }

    fn attempt() -> UnconfirmedPrCreate {
        UnconfirmedPrCreate { branch: "task-branch".into(), commit: COMMIT.into(), started_at: at(100) }
    }

    fn task_with(extra: serde_json::Value) -> Task {
        let mut json = serde_json::json!({
            "id": Uuid::new_v4(),
            "project_id": Uuid::new_v4(),
            "title": "t",
            "description": null,
            "status": TaskStatus::HumanReview,
            "model": "default",
            "planning_mode": false,
            "dependencies": [],
            "workspace_id": null,
            "jj_change_id": null,
            "category": "feature",
            "priority": "medium",
            "complexity": "moderate",
            "impact": "medium",
            "security_severity": "none",
            "phase": "complete",
            "phase_progress": 0,
            "overall_progress": 90,
            "subtasks": [],
            "sequence_number": 0,
            "github_issue_url": null,
            "gitlab_issue_url": null,
            "linear_ticket_id": null,
            "pr_url": null,
            "qa_signoff": null,
            "stuck_since": null,
            "created_at": at(0),
            "updated_at": at(0),
        });
        json.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(json).expect("a task as the backend sends it")
    }

    fn unknown_task() -> Task {
        task_with(serde_json::json!({ "pr_create_unconfirmed": attempt() }))
    }

    fn discarded_entry() -> serde_json::Value {
        serde_json::json!({
            "seq": 1,
            "at": at(200),
            "kind": {
                "type": "pr_create_unconfirmed_discarded",
                "branch": "task-branch",
                "commit": COMMIT,
                "started_at": at(100),
            }
        })
    }

    // ---- 12: the three states

    #[test]
    fn an_unanswered_request_is_unknown() {
        assert_eq!(recovery_state(&unknown_task(), None), Some(RecoveryState::Unknown(attempt())));
    }

    #[test]
    fn a_task_with_nothing_unanswered_shows_no_recovery() {
        assert_eq!(recovery_state(&task_with(serde_json::json!({})), None), None);
    }

    #[test]
    fn a_decision_of_the_person_is_resolved_by_person_and_not_verified() {
        let task = task_with(serde_json::json!({ "activity": [discarded_entry()] }));
        assert_eq!(recovery_state(&task, None), Some(RecoveryState::ResolvedByPerson(attempt())));
        assert!(!matches!(recovery_state(&task, None), Some(RecoveryState::Verified { .. })));
    }

    #[test]
    fn a_pull_request_linked_after_the_recovery_is_verified() {
        let task = task_with(serde_json::json!({ "pr_url": URL, "activity": [discarded_entry()] }));
        let linked = UnconfirmedOutcome::Linked { url: URL.into() };
        assert_eq!(recovery_state(&task, Some(&linked)), Some(RecoveryState::Verified { url: URL.into() }));
        // Later, the linked pull request is just the task's pull request.
        assert_eq!(recovery_state(&task, None), None);
    }

    #[test]
    fn a_link_answer_is_not_verified_unless_the_task_holds_that_link() {
        let task = task_with(serde_json::json!({}));
        let linked = UnconfirmedOutcome::Linked { url: URL.into() };
        assert_eq!(recovery_state(&task, Some(&linked)), None);
    }

    #[test]
    fn an_unanswered_request_outranks_an_earlier_decision() {
        let task = task_with(serde_json::json!({
            "pr_create_unconfirmed": attempt(),
            "activity": [discarded_entry()],
        }));
        assert_eq!(recovery_state(&task, None), Some(RecoveryState::Unknown(attempt())));
    }

    // ---- the confirmation names what was shown

    #[test]
    fn a_confirmation_still_describes_the_attempt_it_was_opened_for() {
        let shown = attempt();
        assert!(still_current(&shown, Some(&attempt())));
        assert!(!still_current(&shown, None), "settled meanwhile");
        let newer = UnconfirmedPrCreate { started_at: at(500), ..attempt() };
        assert!(!still_current(&shown, Some(&newer)), "replaced meanwhile");
        let moved = UnconfirmedPrCreate { commit: "2".repeat(40), ..attempt() };
        assert!(!still_current(&shown, Some(&moved)));
        let other = UnconfirmedPrCreate { branch: "other".into(), ..attempt() };
        assert!(!still_current(&shown, Some(&other)));
    }

    #[test]
    fn discarding_needs_the_acknowledgement_and_an_idle_panel() {
        assert!(!can_discard(false, false));
        assert!(!can_discard(true, true));
        assert!(can_discard(true, false));
    }

    #[test]
    fn only_something_shaped_like_a_pull_request_url_is_offered_to_the_backend() {
        assert!(looks_like_pull_request_url(URL));
        assert!(looks_like_pull_request_url(&format!("  {URL}/  ")));
        for not in ["", "o/r#41", "https://github.com/o/r/issues/41", "https://github.com/o/r/pull/x", "https://github.com/o/r/pull/41/files"] {
            assert!(!looks_like_pull_request_url(not), "{not:?}");
        }
    }

    // ---- what each answer says

    #[test]
    fn each_answer_reads_differently_and_none_claims_github_proved_a_discard() {
        let notes = [
            outcome_note(&UnconfirmedOutcome::Linked { url: URL.into() }),
            outcome_note(&UnconfirmedOutcome::Discarded { durable: true }),
            outcome_note(&UnconfirmedOutcome::Discarded { durable: false }),
            outcome_note(&UnconfirmedOutcome::PullRequestFound { url: URL.into() }),
            outcome_note(&UnconfirmedOutcome::NotThisPullRequest { reason: "wrong branch".into() }),
            outcome_note(&UnconfirmedOutcome::Stale { reason: "changed".into() }),
        ];
        for (i, a) in notes.iter().enumerate() {
            assert!(!a.text.is_empty());
            for b in &notes[i + 1..] {
                assert_ne!(a.text, b.text);
            }
        }
        assert_eq!(notes[0].tone, Tone::Good);
        assert!(notes[0].text.contains("checked"), "{}", notes[0].text);
        let discarded = &notes[1].text;
        assert!(discarded.contains("You confirmed"), "{discarded}");
        assert!(discarded.contains("GitHub did not confirm"), "{discarded}");
        assert!(discarded.contains("Nothing was sent"), "{discarded}");
        assert!(notes[2].text.contains("could not confirm that the change"), "{}", notes[2].text);
        assert!(notes[3].text.contains(URL));
        assert!(notes[4].text.contains("wrong branch"));
    }

    #[test]
    fn only_a_found_pull_request_offers_a_url_to_link() {
        assert_eq!(found_url(&UnconfirmedOutcome::PullRequestFound { url: URL.into() }), Some(URL));
        assert_eq!(found_url(&UnconfirmedOutcome::Discarded { durable: true }), None);
        assert_eq!(found_url(&UnconfirmedOutcome::Stale { reason: "x".into() }), None);
    }

    #[test]
    fn the_task_an_answer_carries_is_applied_to_the_board() {
        let answer = ResolveUnconfirmedOutcome {
            task: Some(unknown_task()),
            result: UnconfirmedOutcome::Discarded { durable: true },
        };
        assert!(answer_task(&answer).is_some());
        let gone = ResolveUnconfirmedOutcome { task: None, result: UnconfirmedOutcome::Discarded { durable: true } };
        assert!(answer_task(&gone).is_none());
    }

    // ---- the wire

    #[test]
    fn the_backend_reads_what_the_frontend_sends_and_the_frontend_reads_the_answer() {
        assert_eq!(
            serde_json::to_value(UnconfirmedResolution::LinkExisting { pr_url: URL.into() }).unwrap(),
            serde_json::json!({"kind":"link_existing","pr_url":URL})
        );
        assert_eq!(
            serde_json::to_value(UnconfirmedResolution::ConfirmNoPullRequest { acknowledged: true }).unwrap(),
            serde_json::json!({"kind":"confirm_no_pull_request","acknowledged":true})
        );
        assert_eq!(
            serde_json::to_value(attempt()).unwrap(),
            serde_json::json!({"branch":"task-branch","commit":COMMIT,"started_at":at(100)})
        );
        let answer: ResolveUnconfirmedOutcome = serde_json::from_value(serde_json::json!({
            "task": null,
            "result": {"outcome":"pull_request_found","url":URL}
        }))
        .unwrap();
        assert_eq!(answer.result, UnconfirmedOutcome::PullRequestFound { url: URL.into() });
    }

    #[test]
    fn a_task_without_the_field_loads_as_nothing_unanswered() {
        assert_eq!(task_with(serde_json::json!({})).pr_create_unconfirmed, None);
        assert_eq!(unknown_task().pr_create_unconfirmed, Some(attempt()));
    }
}
