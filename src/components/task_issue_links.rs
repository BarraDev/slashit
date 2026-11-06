// SPDX-License-Identifier: MPL-2.0
//! The GitHub Issues a Task links, in the task drawer: link one by its web
//! address, open it in the browser, or unlink it.
//!
//! The backend decides every change and confirms the Issue with GitHub before
//! it is linked. GitHub is asked once, at link time, so the state shown is the
//! one recorded then; nothing here refreshes it.

use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

use crate::models::{ExternalRef, Task};
use crate::services::{link_github_issue, open_url_external, remove_external_ref};

/// One GitHub Issue reference, as the section shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueRow {
    pub url: String,
    pub label: String,
    pub state: String,
}

/// The GitHub Issue references among a Task's references, in their order.
pub fn issue_rows(refs: &[ExternalRef]) -> Vec<IssueRow> {
    refs.iter()
        .filter_map(|r| match r {
            ExternalRef::GithubIssue {
                url,
                number,
                repo,
                state,
            } => Some(IssueRow {
                url: url.clone(),
                label: format!("{repo} #{number}"),
                state: state_label(state.as_deref()).to_string(),
            }),
            _ => None,
        })
        .collect()
}

/// What a recorded state reads as. A reference recorded before states were
/// kept has none, and is said so rather than shown as open.
pub fn state_label(state: Option<&str>) -> &'static str {
    match state {
        Some("OPEN") => "Open",
        Some("CLOSED") => "Closed",
        _ => "State not recorded",
    }
}

/// The GitHub Issue reference at `url`, with its position, for the removal
/// the drawer sends.
pub fn unlink_target(refs: &[ExternalRef], url: &str) -> Option<(usize, ExternalRef)> {
    refs.iter().enumerate().find_map(|(index, r)| match r {
        ExternalRef::GithubIssue { url: u, .. } if u == url => Some((index, r.clone())),
        _ => None,
    })
}

/// The GitHub Issues section of the task drawer.
#[component]
pub fn TaskIssueLinks(
    task_id: Uuid,
    #[prop(into)] task: Signal<Option<Task>>,
    /// Apply a task record a command returned.
    apply_task: Callback<Task>,
) -> impl IntoView {
    let rows = Memo::new(move |_| {
        task.with(|t| {
            t.as_ref()
                .map(|t| issue_rows(&t.external_refs))
                .unwrap_or_default()
        })
    });
    let address = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let link = move || {
        if busy.get_untracked() {
            return;
        }
        let typed = address.get_untracked();
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result = link_github_issue(task_id.to_string(), typed).await;
            busy.try_set(false);
            match result {
                Ok(Some(updated)) => {
                    apply_task.try_run(updated);
                    address.try_set(String::new());
                }
                Ok(None) => {}
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };

    let unlink = move |url: String| {
        if busy.get_untracked() {
            return;
        }
        // The position and the reference are read from the record as it is
        // now. The backend removes the reference only if that position still
        // holds this one, since an earlier removal moves the references after it.
        let Some((index, expected)) =
            task.with_untracked(|t| t.as_ref().and_then(|t| unlink_target(&t.external_refs, &url)))
        else {
            return;
        };
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result = remove_external_ref(task_id.to_string(), index, expected).await;
            busy.try_set(false);
            match result {
                Ok(Some(updated)) => {
                    apply_task.try_run(updated);
                }
                Ok(None) => {}
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };

    let open = move |url: String| {
        spawn_local(async move {
            if let Err(e) = open_url_external(url).await {
                error.try_set(Some(e));
            }
        });
    };

    view! {
        <section data-testid="task-drawer-issues" class="space-y-2">
            <h3 class="text-xs font-semibold uppercase tracking-wide text-white/40">"GitHub issues"</h3>
            <ul class="space-y-1.5">
                <For
                    each=move || rows.get()
                    key=|row| row.url.clone()
                    children=move |row| {
                        let open_url = row.url.clone();
                        let unlink_url = row.url.clone();
                        view! {
                            <li data-testid="task-drawer-issue" class="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
                                <span class="text-white/80">{row.label}</span>
                                <span data-testid="task-drawer-issue-state" class="text-xs text-white/50">{row.state}</span>
                                <button
                                    type="button"
                                    data-testid="task-drawer-issue-open"
                                    class="text-xs text-blue-300 hover:text-blue-200"
                                    on:click=move |_| open(open_url.clone())
                                >
                                    "Open"
                                </button>
                                <button
                                    type="button"
                                    data-testid="task-drawer-issue-unlink"
                                    class="text-xs text-red-300 hover:text-red-200 disabled:opacity-50"
                                    disabled=move || busy.get()
                                    on:click=move |_| unlink(unlink_url.clone())
                                >
                                    "Unlink"
                                </button>
                            </li>
                        }
                    }
                />
            </ul>
            <form
                class="flex flex-wrap items-center gap-2"
                on:submit=move |ev| {
                    ev.prevent_default();
                    link();
                }
            >
                <label class="sr-only" for="task-issue-address">"GitHub issue address"</label>
                <input
                    id="task-issue-address"
                    type="text"
                    data-testid="task-drawer-issue-input"
                    placeholder="https://github.com/owner/repo/issues/1"
                    class="flex-1 min-w-0 px-2 py-1.5 rounded-lg text-sm bg-white/5 border border-white/10 text-white/80 placeholder:text-white/30"
                    prop:value=move || address.get()
                    on:input=move |ev| address.set(event_target_value(&ev))
                    disabled=move || busy.get()
                />
                <button
                    type="submit"
                    data-testid="task-drawer-issue-link"
                    class="px-3 py-1.5 rounded-lg text-sm bg-white/5 text-white/70 hover:bg-white/10 disabled:opacity-50"
                    disabled=move || busy.get() || address.with(|a| a.trim().is_empty())
                >
                    {move || if busy.get() { "Checking…" } else { "Link issue" }}
                </button>
            </form>
            <p class="text-xs text-white/40">
                "The state is the one GitHub reported when the issue was linked. SlashIt does not refresh it."
            </p>
            {move || error.get().map(|e| view! {
                <p data-testid="task-drawer-issue-error" role="alert" class="text-xs text-red-300 whitespace-pre-wrap break-words">{e}</p>
            })}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_github_issue_references_are_listed_with_their_recorded_state() {
        let refs = vec![
            ExternalRef::GithubPr {
                url: "https://github.com/o/r/pull/1".into(),
                number: 1,
                repo: "o/r".into(),
                state: Some("OPEN".into()),
            },
            ExternalRef::GithubIssue {
                url: "https://github.com/o/r/issues/2".into(),
                number: 2,
                repo: "o/r".into(),
                state: Some("CLOSED".into()),
            },
            ExternalRef::GithubIssue {
                url: "https://github.com/o/r/issues/3".into(),
                number: 3,
                repo: "o/r".into(),
                state: None,
            },
        ];
        assert_eq!(
            issue_rows(&refs),
            vec![
                IssueRow {
                    url: "https://github.com/o/r/issues/2".into(),
                    label: "o/r #2".into(),
                    state: "Closed".into(),
                },
                IssueRow {
                    url: "https://github.com/o/r/issues/3".into(),
                    label: "o/r #3".into(),
                    state: "State not recorded".into(),
                },
            ]
        );
    }

    #[test]
    fn unlinking_names_the_reference_and_its_position_at_the_time() {
        let refs = vec![
            ExternalRef::GithubIssue {
                url: "https://github.com/o/r/issues/2".into(),
                number: 2,
                repo: "o/r".into(),
                state: Some("OPEN".into()),
            },
            ExternalRef::GithubIssue {
                url: "https://github.com/o/r/issues/3".into(),
                number: 3,
                repo: "o/r".into(),
                state: Some("OPEN".into()),
            },
        ];
        let (index, expected) = unlink_target(&refs, "https://github.com/o/r/issues/3").unwrap();
        assert_eq!(index, 1);
        assert_eq!(expected, refs[1]);
        assert_eq!(unlink_target(&refs, "https://github.com/o/r/issues/9"), None);
    }

    #[test]
    fn an_unrecognized_state_is_not_shown_as_open() {
        assert_eq!(state_label(Some("OPEN")), "Open");
        assert_eq!(state_label(Some("MERGED")), "State not recorded");
        assert_eq!(state_label(None), "State not recorded");
    }
}
