//! Leftover Task Checkouts and task branches of one project: what no task
//! owns any more, and an explicit way to reclaim each.
//!
//! Nothing runs when the panel opens. The scan is a button, it only reads,
//! and every removal is a button on its own row that asks to be confirmed.
//! An item the backend would refuse shows why instead of a button. After any
//! reclaim, refused or not, the project is scanned again, because the answer
//! may have changed.

use crate::components::toast;
use crate::models::orphans::{OrphanBranch, OrphanCheckout, OrphanScan};
use crate::services::orphans_service::{
    reclaim_orphan_branch, reclaim_orphan_checkout, scan_orphans,
};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashMap;

#[component]
pub fn OrphanReclaim(project_id: String) -> impl IntoView {
    let scan = RwSignal::new(None::<OrphanScan>);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    // The item the person is being asked to confirm, and why a reclaim of
    // an item was refused.
    let confirming = RwSignal::new(None::<String>);
    let refused = RwSignal::new(HashMap::<String, String>::new());
    let pid = StoredValue::new(project_id);

    let run_scan = move || {
        let id = pid.get_value();
        if id.is_empty() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            match scan_orphans(id).await {
                Ok(found) => {
                    error.set(None);
                    scan.set(Some(found));
                }
                // A scan that failed is not an empty one.
                Err(e) => {
                    scan.set(None);
                    error.set(Some(e));
                }
            }
            busy.set(false);
        });
    };

    let reclaim = move |key: String, is_branch: bool| {
        let id = pid.get_value();
        busy.set(true);
        confirming.set(None);
        spawn_local(async move {
            let name = key.clone();
            let answer = if is_branch {
                reclaim_orphan_branch(id, name).await
            } else {
                reclaim_orphan_checkout(id, name).await
            };
            match answer {
                Ok(()) => {
                    refused.update(|r| {
                        r.remove(&key);
                    });
                    toast::success(format!("Reclaimed {key}"));
                }
                Err(why) => refused.update(|r| {
                    r.insert(key, why);
                }),
            }
            busy.set(false);
            run_scan();
        });
    };

    let row = move |key: String, kind: &'static str, title: String, details: Vec<String>, refusal: Option<String>, is_branch: bool| {
        let testid = format!("orphan-{kind}");
        let key_for_confirm = key.clone();
        let key_for_ask = key.clone();
        let key_for_err = key.clone();
        let verb = if is_branch { "Delete branch" } else { "Remove checkout" };
        view! {
            <div class="p-3 rounded-lg bg-white/5 border border-white/10 space-y-1" data-testid=testid>
                <p class="text-xs uppercase tracking-wide text-white/40">{kind}</p>
                <p class="text-sm text-white/90 font-mono break-all">{title}</p>
                {details.into_iter().map(|d| view! { <p class="text-xs text-white/60">{d}</p> }).collect_view()}
                {match refusal {
                    Some(why) => view! {
                        <p class="text-xs text-amber-400" data-testid="orphan-refusal">{format!("Not reclaimable: {why}")}</p>
                    }.into_any(),
                    None => view! {
                        <div class="flex gap-2 pt-1">
                            {move || if confirming.get().as_deref() == Some(key_for_ask.as_str()) {
                                let k = key_for_confirm.clone();
                                view! {
                                    <button class="px-3 py-1 text-xs rounded bg-red-500/20 text-red-300" data-testid="orphan-confirm"
                                        disabled=move || busy.get()
                                        on:click=move |_| reclaim(k.clone(), is_branch)>{format!("Confirm: {verb}")}</button>
                                    <button class="px-3 py-1 text-xs rounded bg-white/10 text-white/70"
                                        on:click=move |_| confirming.set(None)>"Cancel"</button>
                                }.into_any()
                            } else {
                                let k = key_for_ask.clone();
                                view! {
                                    <button class="px-3 py-1 text-xs rounded bg-white/10 text-white/80" data-testid="orphan-reclaim"
                                        disabled=move || busy.get()
                                        on:click=move |_| confirming.set(Some(k.clone()))>{verb}</button>
                                }.into_any()
                            }}
                        </div>
                    }.into_any(),
                }}
                {move || refused.get().get(&key_for_err).cloned().map(|why| view! {
                    <p class="text-xs text-red-400" data-testid="orphan-reclaim-refused">{why}</p>
                })}
            </div>
        }
    };

    let checkout_view = move |c: OrphanCheckout| {
        let mut details = vec![c.presence.label().to_string(), c.work_label().to_string()];
        details.push(match &c.branch {
            Some(b) => format!("Branch {b} (kept when the checkout is removed)"),
            None => "Detached HEAD".to_string(),
        });
        row(c.path.clone(), "checkout", c.path, details, c.refusal, false)
    };
    let branch_view = move |b: OrphanBranch| {
        let mut details = vec![b.work_label()];
        if let Some(tip) = &b.tip {
            details.push(format!("At {}", tip.get(..12).unwrap_or(tip)));
        }
        if let Some(path) = &b.checked_out_at {
            details.push(format!("Checked out at {path}"));
        }
        row(b.name.clone(), "branch", b.name, details, b.refusal, true)
    };

    view! {
        <div class="space-y-4" data-testid="orphan-reclaim">
            <div>
                <h2 class="text-lg font-semibold text-white/90">"Leftover task checkouts and branches"</h2>
                <p class="text-sm text-white/50">
                    "Checkouts under SlashIt's own directory and task branches that no task owns. \
                     Scanning only reads. Nothing is removed unless you confirm it, and a checkout \
                     with unsaved work or a branch whose commits exist nowhere else is never removed."
                </p>
            </div>
            <button class="px-3 py-1.5 text-sm rounded bg-white/10 text-white/80" data-testid="orphan-scan"
                disabled=move || busy.get() on:click=move |_| run_scan()>"Scan for leftovers"</button>
            {move || error.get().map(|e| view! {
                <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm" data-testid="orphan-scan-error">
                    {format!("The scan failed, so nothing is known to be safe: {e}")}
                </div>
            })}
            {move || scan.get().map(|found| {
                if found.checkouts.is_empty() && found.branches.is_empty() {
                    return view! { <p class="text-sm text-white/60" data-testid="orphan-none">"No leftover checkouts or task branches."</p> }.into_any();
                }
                view! {
                    <div class="space-y-2" data-testid="orphan-list">
                        <For each=move || found.checkouts.clone() key=|c| c.path.clone() children=checkout_view />
                        <For each={let b = found.branches.clone(); move || b.clone()} key=|b| b.name.clone() children=branch_view />
                    </div>
                }.into_any()
            })}
        </div>
    }
}
