use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::services::{jj_get_workspace_status, JjStatus};

#[component]
pub fn JjStatus(workspace_path: String) -> impl IntoView {
    let (status, set_status) = signal(None::<JjStatus>);
    let (loading, set_loading) = signal(false);

    let load_status = move || {
        let workspace_path = workspace_path.clone();
        let set_status = set_status;
        let set_loading = set_loading;

        set_loading.set(true);
        spawn_local(async move {
            match jj_get_workspace_status(workspace_path).await {
                Ok(s) => {
                    set_status.set(Some(s));
                    set_loading.set(false);
                }
                Err(e) => {
                    eprintln!("Failed to get JJ status: {}", e);
                    set_loading.set(false);
                }
            }
        });
    };

    load_status();

    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
            <div class="px-4 py-3 border-b border-white/5">
                <div class="flex items-center justify-between">
                    <div class="flex items-center gap-2">
                        <svg class="w-5 h-5 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 10V3L4 14h7v7l9-11h-7z" />
                        </svg>
                        <h3 class="font-semibold text-white/90">"Jujutsu Status"</h3>
                    </div>

                    <button
                        disabled=move || loading.get()
                        on:click=move |_| load_status()
                        class="p-2 rounded-lg bg-white/5 hover:bg-white/10 disabled:bg-white/5 disabled:text-white/20 text-white/60 transition-colors"
                        title="Refresh status"
                        aria-label="Refresh status"
                    >
                        <svg class=format!("w-4 h-4 {}", if loading.get() { "animate-spin" } else { "" }) fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                        </svg>
                    </button>
                </div>
            </div>

            <div class="p-4">
                {move || match status.get() {
                    None => view! {
                        <div class="flex items-center justify-center py-8">
                            <div class="flex items-center gap-2 text-white/40">
                                <svg class="w-5 h-5 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                </svg>
                                <span class="text-sm">"Loading..."</span>
                            </div>
                        </div>
                    }.into_any(),
                    Some(s) => view! {
                        <div class="space-y-3">
                            {if let Some(change_id) = &s.current_change_id {
                                view! {
                                    <div class="flex items-center justify-between py-2 px-3 rounded-lg bg-blue-500/10 border border-blue-500/20">
                                        <span class="text-sm text-white/60">"Current Change"</span>
                                        <span class="text-sm font-mono text-blue-300">{change_id.clone()}</span>
                                    </div>
                                }.into_any()
                            } else {
                                ().into_any()
                            }}

                            <div class="grid grid-cols-2 gap-3">
                                <div class=format!(
                                    "flex items-center justify-between py-2 px-3 rounded-lg border {}",
                                    if s.pending_changes {
                                        "bg-yellow-500/10 border-yellow-500/20"
                                    } else {
                                        "bg-white/5 border-white/10"
                                    }
                                )>
                                    <span class="text-sm text-white/60">"Pending"</span>
                                    <span class=format!(
                                        "text-sm font-medium {}",
                                        if s.pending_changes { "text-yellow-300" } else { "text-white/40" }
                                    )>
                                        {if s.pending_changes { "Yes" } else { "No" }}
                                    </span>
                                </div>

                                <div class=format!(
                                    "flex items-center justify-between py-2 px-3 rounded-lg border {}",
                                    if s.conflicted {
                                        "bg-red-500/10 border-red-500/20"
                                    } else {
                                        "bg-white/5 border-white/10"
                                    }
                                )>
                                    <span class="text-sm text-white/60">"Conflicted"</span>
                                    <span class=format!(
                                        "text-sm font-medium {}",
                                        if s.conflicted { "text-red-300" } else { "text-white/40" }
                                    )>
                                        {if s.conflicted { "Yes" } else { "No" }}
                                    </span>
                                </div>
                            </div>
                        </div>
                    }.into_any()
                }}
            </div>
        </div>
    }
}
