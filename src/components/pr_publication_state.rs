// SPDX-License-Identifier: MPL-2.0
//! The decisions behind the Publish to PR panel, free of signals and services
//! so the host can test each rule.
//!
//! The panel shows a confirmation for one commit, one branch and one pull
//! request. These functions decide whether that confirmation still describes
//! the live task, whether a publication answer belongs to the request that was
//! sent, and which publication record the panel should show. The backend
//! revalidates everything; these rules stop the panel from showing or sending
//! a commit the person did not confirm.

use crate::models::PrPublication;

/// What the person was shown when the confirmation opened. The publish action
/// sends exactly these values and nothing read later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishConfirmation {
    pub commit: String,
    pub branch: String,
    pub pr_url: String,
}

/// The live task state a confirmation is checked against. `None` means the
/// task no longer has that value, which also invalidates the confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveTarget<'a> {
    pub tip: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub pr_url: Option<&'a str>,
}

/// Whether the confirmation still describes what a publish would send. Any
/// change to the branch tip, the branch name or the pull request URL, and any
/// missing value, means the person has not confirmed what would be published.
pub fn confirmation_still_matches(shown: &PublishConfirmation, live: LiveTarget<'_>) -> bool {
    live.tip == Some(shown.commit.as_str())
        && live.branch == Some(shown.branch.as_str())
        && live.pr_url == Some(shown.pr_url.as_str())
}

/// Whether a publication answer is for the commit and pull request the request
/// named. A reconciled earlier record is an answer for another commit or another
/// pull request, and must not be shown or taken as success for the requested one.
pub fn answers_request(
    answer: &PrPublication,
    requested_commit: &str,
    requested_pr_url: &str,
) -> bool {
    answer.commit == requested_commit && answer.pr_url == requested_pr_url
}

/// The record the panel shows: the later of the task's record and the panel's
/// last answer, by `updated_at`. When the timestamps are equal the task's
/// record wins, because the task is what the board reloads from storage.
pub fn shown_record<'a>(
    from_task: Option<&'a PrPublication>,
    last_answer: Option<&'a PrPublication>,
) -> Option<&'a PrPublication> {
    match (from_task, last_answer) {
        (Some(task), Some(answer)) if answer.updated_at > task.updated_at => Some(answer),
        (Some(task), _) => Some(task),
        (None, answer) => answer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::PublicationState;
    use chrono::{DateTime, Utc};

    const TIP: &str = "1111111111111111111111111111111111111111";
    const NEXT: &str = "2222222222222222222222222222222222222222";
    const BRANCH: &str = "task-branch";
    const PR: &str = "https://github.com/BarraDev/slashit/pull/9";

    fn shown() -> PublishConfirmation {
        PublishConfirmation {
            commit: TIP.into(),
            branch: BRANCH.into(),
            pr_url: PR.into(),
        }
    }

    fn live<'a>(
        tip: Option<&'a str>,
        branch: Option<&'a str>,
        pr_url: Option<&'a str>,
    ) -> LiveTarget<'a> {
        LiveTarget {
            tip,
            branch,
            pr_url,
        }
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(seconds, 0).expect("valid timestamp")
    }

    fn record(commit: &str, updated_at: i64) -> PrPublication {
        PrPublication {
            request_id: "00000000-0000-0000-0000-000000000000".into(),
            commit: commit.into(),
            branch: BRANCH.into(),
            pr_url: PR.into(),
            remote_before: None,
            state: PublicationState::Published,
            requested_at: at(0),
            updated_at: at(updated_at),
        }
    }

    #[test]
    fn an_unchanged_live_target_keeps_the_confirmation() {
        assert!(confirmation_still_matches(
            &shown(),
            live(Some(TIP), Some(BRANCH), Some(PR))
        ));
    }

    #[test]
    fn a_changed_tip_invalidates_the_confirmation() {
        assert!(!confirmation_still_matches(
            &shown(),
            live(Some(NEXT), Some(BRANCH), Some(PR))
        ));
    }

    #[test]
    fn a_changed_pull_request_or_branch_invalidates_the_confirmation() {
        assert!(!confirmation_still_matches(
            &shown(),
            live(
                Some(TIP),
                Some(BRANCH),
                Some("https://github.com/BarraDev/slashit/pull/10")
            )
        ));
        assert!(!confirmation_still_matches(
            &shown(),
            live(Some(TIP), Some("other-branch"), Some(PR))
        ));
    }

    #[test]
    fn a_missing_tip_branch_or_pull_request_invalidates_the_confirmation() {
        assert!(!confirmation_still_matches(
            &shown(),
            live(None, Some(BRANCH), Some(PR))
        ));
        assert!(!confirmation_still_matches(
            &shown(),
            live(Some(TIP), None, Some(PR))
        ));
        assert!(!confirmation_still_matches(
            &shown(),
            live(Some(TIP), Some(BRANCH), None)
        ));
    }

    #[test]
    fn an_answer_counts_only_for_the_commit_that_was_requested() {
        let reconciled_earlier = record(TIP, 10);
        assert!(answers_request(&reconciled_earlier, TIP, PR));
        assert!(!answers_request(&reconciled_earlier, NEXT, PR));
        // The same commit on another pull request is a different request.
        assert!(!answers_request(
            &reconciled_earlier,
            TIP,
            "https://github.com/BarraDev/slashit/pull/10"
        ));
    }

    #[test]
    fn a_newer_local_answer_is_shown_over_an_older_task_record() {
        let task = record(TIP, 10);
        let answer = record(NEXT, 20);
        assert_eq!(
            shown_record(Some(&task), Some(&answer)).map(|r| r.commit.as_str()),
            Some(NEXT)
        );
    }

    #[test]
    fn a_newer_task_record_is_shown_over_an_older_local_answer() {
        let task = record(TIP, 30);
        let answer = record(NEXT, 20);
        assert_eq!(
            shown_record(Some(&task), Some(&answer)).map(|r| r.commit.as_str()),
            Some(TIP)
        );
    }

    #[test]
    fn equal_timestamps_prefer_the_task_record() {
        // The task's record is what storage holds. Ties go to it so the panel
        // does not flip to a local answer that a reload would contradict.
        let task = record(TIP, 40);
        let answer = record(NEXT, 40);
        assert_eq!(
            shown_record(Some(&task), Some(&answer)).map(|r| r.commit.as_str()),
            Some(TIP)
        );
    }

    #[test]
    fn a_single_source_is_shown_as_is() {
        let only = record(TIP, 5);
        assert_eq!(shown_record(Some(&only), None), Some(&only));
        assert_eq!(shown_record(None, Some(&only)), Some(&only));
        assert_eq!(shown_record(None, None), None);
    }
}
