// SPDX-License-Identifier: MPL-2.0
//! Publish to PR: the explicit action that pushes the task branch's current
//! commit to the task's existing pull request.
//!
//! It is a decision of its own. It does not approve the task, does not change
//! the Human Review record, and covers only the commit shown in its
//! confirmation, where its diffs can be inspected. The backend revalidates
//! everything and records the outcome; this component only asks, confirms, and
//! shows what the record says. The decisions the panel relies on (whether a confirmation still matches, which
//! answer belongs to a request, which record is newer) live in
//! [`super::pr_publication_state`] so they can be tested on the host.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use super::diff_viewer::DiffViewer;
use super::pr_publication_state::{
    answers_request, confirmation_still_matches, inspected_remote_head, shown_record, Approval,
    LiveTarget, PublishConfirmation,
};
use crate::models::{
    PrPublication, PublicationInspection, PublicationState, RevisionComparison, Task, TaskStatus,
};
use crate::services::human_review_service::get_approval_status;
use crate::services::pr_service::{inspect_task_publication, publish_task_to_pr};

/// Shown when the live branch, commit, pull request or approval no longer
/// matches the inspection. Nothing is published in that case.
const CHANGED_NOTICE: &str =
    "The branch, the pull request or the approval changed while you were confirming, so \
     nothing was published. Review the commit and confirm again.";

/// The first twelve characters of a commit, as the drawer names commits.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// What the approval says about `commit`, in words a person can act on.
fn approval_note(task: &Task, commit: &str) -> &'static str {
    if !task.human_review.is_approved() {
        "The task is not approved. Publishing this commit does not approve it."
    } else if task.human_review.approved_tip() == Some(commit) {
        "The approval covers this commit. Publishing does not change the approval."
    } else {
        "The approval covers an earlier commit, so it does not cover this one. Publishing does not \
         approve it."
    }
}

/// The task's live branch name, pull request URL and latest approval.
fn task_target(task: &Option<Task>) -> (Option<String>, Option<String>, Option<Approval>) {
    match task {
        Some(t) => {
            let approval = t.human_review.latest_approved_tip().map(|tip| {
                (tip.to_string(), t.human_review.approved_tip() == Some(tip))
            });
            (t.branch_name.clone(), t.pr_url.clone(), approval)
        }
        None => (None, None, None),
    }
}

/// Whether `shown` still describes the commit `tip` and the task `target`.
fn still_matches(
    shown: &PublishConfirmation,
    tip: Option<&str>,
    target: &(Option<String>, Option<String>, Option<Approval>),
) -> bool {
    let (branch, pr_url, approval) = target;
    confirmation_still_matches(
        shown,
        LiveTarget {
            tip,
            branch: branch.as_deref(),
            pr_url: pr_url.as_deref(),
            approval: approval.as_ref().map(|(tip, current)| (tip.as_str(), *current)),
        },
    )
}

/// One labelled comparison with the candidate: which two revisions, and the diff.
fn comparison_view(
    testid: &'static str,
    title: &'static str,
    from_label: &'static str,
    comparison: RevisionComparison,
    candidate: &str,
) -> AnyView {
    let RevisionComparison { from, from_is_ancestor, patch, stat, unavailable } = comparison;
    let body = match unavailable {
        Some(reason) => view! {
            <p class="text-amber-200">{format!("This diff could not be read: {reason}")}</p>
        }
        .into_any(),
        None if patch.trim().is_empty() => view! {
            <p class="text-white/60">"No content differences between these two revisions."</p>
        }
        .into_any(),
        None => view! { <DiffViewer diff=patch stat=stat /> }.into_any(),
    };
    view! {
        <details data-testid=testid class="rounded-md border border-white/10 p-2">
            <summary class="cursor-pointer font-medium text-white/90">{title}</summary>
            <div class="mt-2 space-y-1">
                <p class="break-all">{from_label} ": " <code>{from}</code></p>
                <p class="break-all">"Candidate: " <code>{candidate.to_string()}</code></p>
                {(from_is_ancestor != Some(true)).then(|| view! {
                    <p class="text-amber-200">
                        "The candidate is not known to descend from this revision. The diff is the content difference between the two commits."
                    </p>
                })}
                {body}
            </div>
        </details>
    }
    .into_any()
}

/// The three comparisons a person can open before confirming.
fn comparisons_view(inspection: PublicationInspection) -> AnyView {
    let candidate = inspection.commit.clone();
    let own_title = if inspection.is_merge {
        "Candidate commit changes (merge, against its first parent)"
    } else {
        "Candidate commit changes"
    };
    let approval_title = if inspection.approval_is_current {
        "Changes since the current approval"
    } else {
        "Changes since the previous approval (no longer current)"
    };
    let since_approval = match inspection.since_approval {
        Some(c) => comparison_view("publish-to-pr-diff-approved", approval_title, "Approved revision", c, &candidate),
        None => view! {
            <p data-testid="publish-to-pr-no-approval">"No approved revision is recorded for this task, so there is no approval to compare with."</p>
        }
        .into_any(),
    };
    let since_remote = match inspection.since_remote {
        Some(c) => comparison_view(
            "publish-to-pr-diff-remote",
            "Changes against the remote pull request head",
            "Remote head observed now",
            c,
            &candidate,
        ),
        None => {
            let reason = inspection.remote_unavailable.unwrap_or_default();
            view! {
                <p data-testid="publish-to-pr-no-remote" class="text-amber-200">
                    {format!("The remote head could not be compared, so this commit cannot be published yet: {reason}")}
                </p>
            }
            .into_any()
        }
    };
    view! {
        <div class="space-y-1">
            {comparison_view("publish-to-pr-diff-commit", own_title, "Parent revision", inspection.own_changes, &candidate)}
            {since_approval}
            {since_remote}
        </div>
    }
    .into_any()
}

/// One line describing a recorded publication.
fn status_line(record: PrPublication) -> AnyView {
    let commit = short(&record.commit).to_string();
    match record.state {
        PublicationState::Pending => view! {
            <p data-testid="publish-to-pr-pending" class="text-xs text-amber-200">
                {format!("Publishing {commit} is being recorded. Nothing else can be published until its outcome is confirmed.")}
            </p>
        }
        .into_any(),
        PublicationState::Published => view! {
            <p data-testid="publish-to-pr-published" class="text-xs font-medium text-emerald-300">
                {format!("Published {commit} to the pull request. The approval was not changed.")}
            </p>
        }
        .into_any(),
        PublicationState::Refused { reason } => view! {
            <p data-testid="publish-to-pr-refused" class="text-xs text-amber-200">
                {format!("Not published: {reason}")}
            </p>
        }
        .into_any(),
        PublicationState::Unknown { reason } => view! {
            <p data-testid="publish-to-pr-unknown" class="text-xs text-amber-200">
                {format!(
                    "Publishing {commit} has an unknown outcome: {reason}. Publish again to confirm it against GitHub."
                )}
            </p>
        }
        .into_any(),
    }
}

/// The Publish to PR control and its confirmation, for a task with a branch
/// and a pull request.
#[component]
pub fn PublishToPrPanel(
    task_id: Uuid,
    task: Memo<Option<Task>>,
    /// Reload the board after a publication was recorded, or after a request
    /// that may have settled an earlier one.
    refresh_tasks: Callback<()>,
) -> impl IntoView {
    // The commit a publication would push: the branch tip as the backend reads it now.
    let tip = RwSignal::new(None::<String>);
    // The inspection the person sees. Frozen when the confirmation opens:
    // publishing sends only its values, never anything read at click time.
    let confirmation = RwSignal::new(None::<PublicationInspection>);
    let inspecting = RwSignal::new(false);
    let notice = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // The last answer this panel received. The task record and this answer are
    // compared by `updated_at` when the panel shows one of them.
    let answered = RwSignal::new(None::<PrPublication>);

    let load_tip = move || {
        spawn_local(async move {
            let answer = get_approval_status(task_id.to_string()).await;
            let _ = tip.try_set(answer.ok().and_then(|s| s.branch_tip));
        });
    };
    Effect::new(move |_| {
        // Re-read the tip whenever the task record changes.
        task.with(|_| ());
        load_tip();
    });

    // While a confirmation is open, any change to the live tip, branch, pull
    // request or approval closes it. The person must review the new state and
    // confirm again.
    Effect::new(move |_| {
        let tip_now = tip.get();
        let target = task.with(task_target);
        // A request in flight already carries the commit the person confirmed; the
        // backend checks it against the branch. Closing the confirmation mid-request
        // would leave a notice that says nothing was published while the push may land.
        if busy.get_untracked() {
            return;
        }
        if let Some(shown_now) = confirmation.get_untracked() {
            if !still_matches(&PublishConfirmation::of(&shown_now), tip_now.as_deref(), &target) {
                confirmation.set(None);
                notice.set(Some(CHANGED_NOTICE.to_string()));
            }
        }
    });

    let open_confirmation = move || {
        inspecting.set(true);
        notice.set(None);
        error.set(None);
        spawn_local(async move {
            let answer = inspect_task_publication(task_id.to_string()).await;
            if inspecting.try_set(false).is_some() {
                return;
            }
            match answer {
                Ok(inspection) => {
                    // The inspection is the newest read of the branch tip. The
                    // rest must still be what the task says now.
                    let Some(target) = task.try_with_untracked(task_target) else {
                        return;
                    };
                    let shown_now = PublishConfirmation::of(&inspection);
                    if still_matches(&shown_now, Some(&inspection.commit), &target) {
                        let _ = tip.try_set(Some(inspection.commit.clone()));
                        let _ = confirmation.try_set(Some(inspection));
                    } else {
                        let _ = notice.try_set(Some(CHANGED_NOTICE.to_string()));
                        load_tip();
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };

    let publish = move |branch: String, commit: String, pr_url: String, remote_head: String| {
        busy.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            let requested = commit.clone();
            let requested_branch = branch.clone();
            let requested_pr = pr_url.clone();
            let answer =
                publish_task_to_pr(task_id.to_string(), branch, commit, pr_url, remote_head).await;
            let _ = busy.try_set(false);
            match answer {
                Ok(record)
                    if answers_request(&record, &requested, &requested_branch, &requested_pr) =>
                {
                    let _ = answered.try_set(Some(record));
                    let _ = confirmation.try_set(None);
                }
                Ok(record) => {
                    // The backend answered for a different commit or branch, for
                    // example an earlier publication it settled first. Show
                    // nothing as published for the commit that was asked for.
                    let _ = error.try_set(Some(format!(
                        "The answer was for {} on {}, not {} on {}. Nothing was published for {}. Review the branch and try again.",
                        short(&record.commit),
                        record.branch,
                        short(&requested),
                        requested_branch,
                        short(&requested),
                    )));
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
            // A request can settle an earlier record even when it fails, so the
            // board is reloaded whatever the answer was.
            refresh_tasks.run(());
        });
    };

    // The record to show: the task's, or this panel's answer when it is newer.
    let shown = move || {
        let from_task = task.with(|t| t.as_ref().and_then(|t| t.pr_publication.clone()));
        let last = answered.get();
        shown_record(from_task.as_ref(), last.as_ref()).cloned()
    };

    let frozen = move || confirmation.get();

    view! {
        <div data-testid="publish-to-pr-panel" class="mt-3 space-y-2 border-t border-white/10 pt-3">
            <p class="text-xs font-semibold text-white/80">"Publish to pull request"</p>
            <p class="text-xs text-white/50">
                "Pushes one commit to this task's existing pull request. It is separate from approval: the task stays in Human Review."
            </p>

            {move || shown().map(status_line)}

            <Show when=move || tip.get().is_some() && confirmation.with(Option::is_none) && task.with(|t| t.as_ref().is_some_and(|t| t.pr_url.is_some() && t.branch_name.is_some() && !matches!(t.status, TaskStatus::Queue | TaskStatus::InProgress | TaskStatus::AiReview)))>
                <button
                    type="button"
                    data-testid="publish-to-pr"
                    class="rounded-md border border-white/20 px-3 py-1.5 text-sm text-white/90 hover:bg-white/5 disabled:opacity-50"
                    disabled=move || inspecting.get()
                    on:click=move |_| open_confirmation()
                >
                    {move || if inspecting.get() { "Reading the changes..." } else { "Publish to PR..." }}
                </button>
            </Show>

            <Show when=move || confirmation.with(Option::is_some)>
                <div data-testid="publish-to-pr-confirm" class="space-y-1 rounded-md border border-white/15 p-3 text-xs text-white/80">
                    <p>
                        {move || frozen().map(|c| c.branch).unwrap_or_default()}
                        " will be pushed to the pull request "
                        {move || frozen().map(|c| c.pr_url).unwrap_or_default()}
                        "."
                    </p>
                    <p data-testid="publish-to-pr-commit">
                        "Commit: "
                        <code>{move || frozen().map(|c| c.commit).unwrap_or_default()}</code>
                    </p>
                    <p>
                        {move || {
                            match (task.with(|t| t.clone()), frozen()) {
                                (Some(t), Some(c)) => approval_note(&t, &c.commit).to_string(),
                                _ => String::new(),
                            }
                        }}
                    </p>
                    {move || frozen().map(comparisons_view)}
                    <p class="text-white/50">
                        "Only this commit is published, and only while the remote head is the one compared here. Review the changes before you confirm. A later commit needs a new publication."
                    </p>
                    <div class="flex gap-2 pt-1">
                        <button
                            type="button"
                            data-testid="publish-to-pr-confirm-button"
                            class="rounded-md bg-white/10 px-3 py-1.5 text-sm text-white hover:bg-white/15 disabled:opacity-50"
                            disabled=move || {
                                busy.get()
                                    || confirmation.with(|c| c.as_ref().and_then(inspected_remote_head).is_none())
                            }
                            on:click=move |_| {
                                let Some(shown_now) = confirmation.get_untracked() else {
                                    return;
                                };
                                let Some(remote_head) = inspected_remote_head(&shown_now).map(str::to_string) else {
                                    return;
                                };
                                let shown_now = PublishConfirmation::of(&shown_now);
                                // Publish only if the live state still matches what was shown.
                                let target = task.with_untracked(task_target);
                                if still_matches(&shown_now, tip.get_untracked().as_deref(), &target) {
                                    publish(shown_now.branch, shown_now.commit, shown_now.pr_url, remote_head);
                                } else {
                                    confirmation.set(None);
                                    notice.set(Some(CHANGED_NOTICE.to_string()));
                                }
                            }
                        >
                            "Publish this commit"
                        </button>
                        <button
                            type="button"
                            class="rounded-md px-3 py-1.5 text-sm text-white/60 hover:text-white"
                            disabled=move || busy.get()
                            on:click=move |_| confirmation.set(None)
                        >
                            "Cancel"
                        </button>
                    </div>
                </div>
            </Show>

            <Show when=move || notice.with(Option::is_some)>
                <p data-testid="publish-to-pr-notice" class="text-xs text-amber-200">
                    {move || notice.get().unwrap_or_default()}
                </p>
            </Show>

            <Show when=move || error.with(Option::is_some)>
                <p data-testid="publish-to-pr-error" class="text-xs text-red-300">
                    {move || error.get().unwrap_or_default()}
                </p>
            </Show>
        </div>
    }
}
