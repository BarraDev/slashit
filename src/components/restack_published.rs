//! The one place a published stacked task is moved onto the branch its parent
//! landed on: a notice in the task drawer's pull request section, with the
//! action that fixes what it says, and a confirmation that spells out the
//! rewrite.
//!
//! The backend decides whether a restack is on offer and does all of it (see
//! `commands::pr::republish`). This only shows what it reports and asks first:
//! a published branch is never rewritten without that confirmation.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::components::toast;
use crate::models::RepublishStatus;
use crate::services::pr_service::{discard_published_restack, get_published_restack_status, restack_published_task};

/// Short name of the action, as the button says it.
pub const ACTION_LABEL: &str = "Restack onto merged parent";

/// The notice's heading and its explanation, from what the backend reported.
pub fn notice(status: &RepublishStatus) -> (String, String) {
    match status {
        RepublishStatus::NeedsRestack { parent_branch, parent_pr, default_branch, rewrites: true, .. } => (
            "Parent merged: this branch needs restacking".to_string(),
            format!(
                "{parent_branch} (pull request #{parent_pr}) was merged into {default_branch} as new \
                 commits. This branch still carries the old ones, so its pull request lists them \
                 again. Moving its base alone would not remove them."
            ),
        ),
        RepublishStatus::NeedsRestack { parent_branch, parent_pr, default_branch, rewrites: false, .. } => (
            "Parent merged: retarget this pull request".to_string(),
            format!(
                "{parent_branch} (pull request #{parent_pr}) was merged into {default_branch} with its \
                 commits unchanged, so this branch needs no rewrite. Only the pull request's base has \
                 to move."
            ),
        ),
        RepublishStatus::Blocked { reason } => {
            ("Parent merged: restacking is not possible yet".to_string(), reason.clone())
        }
        RepublishStatus::Interrupted { detail, .. } => ("A restack did not finish".to_string(), detail.clone()),
    }
}

/// What the confirmation tells the person will happen, one statement per
/// line. `branch` is the task's branch.
pub fn consequences(status: &RepublishStatus, branch: &str) -> Vec<String> {
    match status {
        RepublishStatus::NeedsRestack { default_branch, pr_number, rewrites: false, .. } => vec![
            format!("Pull request #{pr_number} will be retargeted to {default_branch}."),
            format!("{branch} is not rewritten and nothing is pushed."),
        ],
        RepublishStatus::NeedsRestack { parent_branch, default_branch, pr_number, rewrites: true, .. } => vec![
            format!(
                "{branch} will be rewritten onto {default_branch}: its own commits are replayed on \
                 top of what {parent_branch} landed as, and {parent_branch}'s old commits are dropped \
                 from it."
            ),
            "The current tip is saved first, in a backup ref under refs/slashit/republish-backup/, \
             so it can be recovered."
                .to_string(),
            format!(
                "origin's {branch} will then be updated with a guarded force push \
                 (--force-with-lease): it is replaced only if it is still at the tip SlashIt checked. \
                 If anyone else pushed in the meantime, nothing is overwritten."
            ),
            format!("Pull request #{pr_number} will then be retargeted to {default_branch}."),
            "GitHub review comments on rewritten commits may become outdated, and CI will run again."
                .to_string(),
        ],
        RepublishStatus::Interrupted { rewritten: true, .. } => vec![
            format!("{branch} was already rewritten here. It is not rewritten again."),
            format!(
                "origin's {branch} will be updated with a guarded force push (--force-with-lease), \
                 only if it is still at the tip that was approved. If anyone else pushed in the \
                 meantime, nothing is overwritten."
            ),
            "The pull request will then be retargeted if it is not already on the right branch.".to_string(),
            "GitHub review comments on rewritten commits may become outdated, and CI will run again."
                .to_string(),
        ],
        RepublishStatus::Interrupted { rewritten: false, .. } => vec![
            format!("{branch} will be rewritten onto the branch its parent landed on, from the tip that was approved."),
            "The tip is already saved in a backup ref under refs/slashit/republish-backup/.".to_string(),
            format!(
                "origin's {branch} will then be updated with a guarded force push \
                 (--force-with-lease), only if it is still at that tip."
            ),
            "The pull request will then be retargeted, and its review comments on rewritten commits \
             may become outdated."
                .to_string(),
        ],
        RepublishStatus::Blocked { .. } => Vec::new(),
    }
}

/// Whether the notice offers the action: not while the backend reports that
/// a restack is not possible.
pub fn offers_action(status: &RepublishStatus) -> bool {
    !matches!(status, RepublishStatus::Blocked { .. })
}

/// The notice for one task, with its action. Reads the backend's answer when it
/// appears and whenever `reload` changes; asks GitHub only then, never on a
/// timer.
#[component]
pub fn RestackNotice(
    task_id: Uuid,
    branch: Memo<Option<String>>,
    /// Bumped by the section when the person refreshes the pull request.
    reload: RwSignal<u32>,
    /// Read the task list and pull request state again after an action.
    refresh: Callback<()>,
) -> impl IntoView {
    let status = RwSignal::new(None::<RepublishStatus>);
    let confirming = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let load = move || {
        spawn_local(async move {
            match get_published_restack_status(task_id.to_string()).await {
                Ok(found) => status.try_set(found),
                // Not knowing is not a claim either way: show nothing.
                Err(e) => {
                    leptos::logging::warn!("[restack] could not check the parent: {e}");
                    status.try_set(None)
                }
            };
        });
    };
    Effect::new(move |_| {
        reload.track();
        load();
    });

    let run = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match restack_published_task(task_id.to_string()).await {
                Ok(outcome) => {
                    confirming.try_set(false);
                    toast::success(if outcome.rewritten {
                        format!("Restacked onto {}; the previous tip is kept in {}", outcome.base, outcome.backup.unwrap_or_default())
                    } else {
                        format!("Pull request retargeted to {}", outcome.base)
                    });
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            refresh.try_run(());
            busy.try_set(false);
        });
    };
    let discard = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match discard_published_restack(task_id.to_string()).await {
                Ok(()) => {
                    toast::success("Restack discarded; the branch is back at the tip it started from".to_string());
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
            refresh.try_run(());
            busy.try_set(false);
        });
    };

    move || {
        status.get().map(|current| {
            let (title, detail) = notice(&current);
            let offers = offers_action(&current);
            let interrupted = matches!(current, RepublishStatus::Interrupted { .. });
            let lines = consequences(&current, &branch.get().unwrap_or_default());
            let label = if interrupted { "Resume restack" } else { ACTION_LABEL };
            view! {
                <div data-testid="task-drawer-restack" class="rounded-md border border-amber-400/30 bg-amber-400/10 p-2 space-y-2">
                    <p data-testid="task-drawer-restack-title" class="text-sm font-medium text-amber-200">{title}</p>
                    <p data-testid="task-drawer-restack-detail" class="text-xs text-white/70 break-words">{detail}</p>
                    {move || error.get().map(|e| view! {
                        <p data-testid="task-drawer-restack-error" role="alert" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
                    })}
                    <Show when=move || confirming.get() && offers>
                        <div data-testid="task-drawer-restack-confirm" role="group" aria-label="Confirm restack" class="space-y-2">
                            <ul data-testid="task-drawer-restack-consequences" class="list-disc pl-5 space-y-1 text-xs text-white/80">
                                {lines.clone().into_iter().map(|line| view! { <li class="break-words">{line}</li> }).collect_view()}
                            </ul>
                            <div class="flex gap-2">
                                <button
                                    data-testid="task-drawer-restack-cancel"
                                    class="px-2 py-1 rounded-md text-xs bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                                    disabled=move || busy.get()
                                    on:click=move |_| confirming.set(false)
                                >
                                    "Cancel"
                                </button>
                                <button
                                    data-testid="task-drawer-restack-submit"
                                    class="px-2 py-1 rounded-md text-xs font-medium bg-amber-400/20 text-amber-100 hover:bg-amber-400/30 disabled:opacity-50"
                                    disabled=move || busy.get()
                                    on:click=run
                                >
                                    {move || if busy.get() { "Restacking…" } else { "Confirm and restack" }}
                                </button>
                            </div>
                        </div>
                    </Show>
                    <Show when=move || !confirming.get()>
                        <div class="flex gap-2">
                            {offers.then(|| view! {
                                <button
                                    data-testid="task-drawer-restack-action"
                                    class="px-2 py-1 rounded-md text-xs font-medium bg-amber-400/20 text-amber-100 hover:bg-amber-400/30 disabled:opacity-50"
                                    disabled=move || busy.get()
                                    on:click=move |_| confirming.set(true)
                                >
                                    {label}
                                </button>
                            })}
                            {interrupted.then(|| view! {
                                <button
                                    data-testid="task-drawer-restack-discard"
                                    class="px-2 py-1 rounded-md text-xs bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                                    disabled=move || busy.get()
                                    title="Put the branch back at the tip the restack started from. Nothing on GitHub changes."
                                    on:click=discard
                                >
                                    "Discard restack"
                                </button>
                            })}
                        </div>
                    </Show>
                </div>
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn needs(rewrites: bool) -> RepublishStatus {
        RepublishStatus::NeedsRestack {
            parent_branch: "task-parent".to_string(),
            parent_pr: 7,
            default_branch: "main".to_string(),
            pr_number: 21,
            rewrites,
        }
    }

    /// The same JSON the backend's `republish::tests` serializes.
    #[test]
    fn the_backends_status_json_is_read() {
        let json = r#"{"kind":"needs_restack","parent_branch":"task-parent","parent_pr":7,"default_branch":"main","pr_number":21,"rewrites":true}"#;
        assert_eq!(serde_json::from_str::<RepublishStatus>(json).unwrap(), needs(true));
        let blocked = r#"{"kind":"blocked","reason":"dirty"}"#;
        assert_eq!(
            serde_json::from_str::<RepublishStatus>(blocked).unwrap(),
            RepublishStatus::Blocked { reason: "dirty".to_string() }
        );
        let interrupted = r#"{"kind":"interrupted","rewritten":true,"detail":"d"}"#;
        assert_eq!(
            serde_json::from_str::<RepublishStatus>(interrupted).unwrap(),
            RepublishStatus::Interrupted { rewritten: true, detail: "d".to_string() }
        );
    }

    /// The confirmation says everything the product requires of it.
    #[test]
    fn a_rewriting_restack_is_confirmed_with_every_consequence() {
        let text = consequences(&needs(true), "task-branch").join("\n");
        for needle in [
            "task-branch will be rewritten onto main",
            "backup ref",
            "guarded force push",
            "--force-with-lease",
            "nothing is overwritten",
            "Pull request #21 will then be retargeted to main",
            "review comments",
            "CI will run again",
        ] {
            assert!(text.contains(needle), "{needle:?} missing from:\n{text}");
        }
    }

    #[test]
    fn a_retarget_only_says_nothing_is_rewritten_or_pushed() {
        let text = consequences(&needs(false), "task-branch").join("\n");
        assert!(text.contains("retargeted to main"), "{text}");
        assert!(text.contains("not rewritten and nothing is pushed"), "{text}");
        assert!(!text.contains("force"), "{text}");
    }

    #[test]
    fn a_resumed_restack_is_not_described_as_a_new_rewrite() {
        let resumed = RepublishStatus::Interrupted { rewritten: true, detail: String::new() };
        let text = consequences(&resumed, "task-branch").join("\n");
        assert!(text.contains("already rewritten"), "{text}");
        assert!(text.contains("--force-with-lease"), "{text}");
    }

    #[test]
    fn a_blocked_restack_offers_no_action_and_explains_why() {
        let blocked = RepublishStatus::Blocked { reason: "uncommitted changes".to_string() };
        assert!(!offers_action(&blocked));
        assert!(consequences(&blocked, "b").is_empty());
        assert_eq!(notice(&blocked).1, "uncommitted changes");
        assert!(offers_action(&needs(true)));
    }

    #[test]
    fn the_notice_tells_a_rewrite_from_a_retarget() {
        assert!(notice(&needs(true)).0.contains("needs restacking"));
        assert!(notice(&needs(false)).0.contains("retarget"));
        assert!(notice(&needs(true)).1.contains("would not remove them"));
    }
}
