//! Storage settings: choose where a project's board is kept, and move it.
//!
//! Moving state is destructive-adjacent, so the flow is always preview then
//! confirm: picking an option builds a plan, the plan is shown with real file
//! counts and any conflicts, and only an explicit confirmation runs the
//! migration. Nothing moves as a side effect of clicking a radio button.

use crate::components::toast;
use crate::models::state_location::{
    human_bytes, ConflictPolicy, MigrationOutcome, MigrationPlan, StateLocation, StateLocationInfo,
};
use crate::services::state_service;
use leptos::prelude::*;
use leptos::task::spawn_local;

const OPTIONS: [StateLocation; 3] = [
    StateLocation::External,
    StateLocation::InProject,
    StateLocation::Auto,
];

#[component]
pub fn StorageSettings(project_id: String) -> impl IntoView {
    let info = RwSignal::new(None::<StateLocationInfo>);
    let loading = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let pending = RwSignal::new(None::<(StateLocation, MigrationPlan)>);

    let pid = project_id.clone();
    let refresh = move || {
        let pid = pid.clone();
        spawn_local(async move {
            if pid.is_empty() {
                loading.set(false);
                return;
            }
            match state_service::get_state_location(pid).await {
                Ok(loaded) => info.set(Some(loaded)),
                Err(e) => toast::error(format!("Could not read storage settings: {e}")),
            }
            loading.set(false);
        });
    };

    let refresh_for_effect = refresh.clone();
    let _ = Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true;
        }
        refresh_for_effect();
        true
    });

    // Build a plan for the chosen target. Never moves anything.
    let pid_plan = project_id.clone();
    let choose = move |target: StateLocation| {
        let pid = pid_plan.clone();
        busy.set(true);
        spawn_local(async move {
            match state_service::plan_state_migration(pid.clone(), target).await {
                Ok(plan) => {
                    if plan.source_file_count == 0 && !plan.destination_exists {
                        // Nothing to move: just record the preference. The
                        // command requires the timestamp this project's info
                        // was last read at, so a decision based on a stale
                        // read is rejected rather than silently applied.
                        let Some(known_updated_at) = info.get_untracked().map(|i| i.updated_at) else {
                            toast::error(
                                "Storage settings have not finished loading. Try again."
                                    .to_string(),
                            );
                            busy.set(false);
                            return;
                        };
                        match state_service::set_state_location(pid, target, known_updated_at).await
                        {
                            Ok(updated) => {
                                info.set(Some(updated));
                                toast::success("Storage location updated".to_string());
                            }
                            Err(e) => toast::error(e),
                        }
                    } else {
                        pending.set(Some((target, plan)));
                    }
                }
                Err(e) => toast::error(format!("Could not prepare the move: {e}")),
            }
            busy.set(false);
        });
    };

    let pid_apply = project_id.clone();
    let apply = move |target: StateLocation, policy: Option<ConflictPolicy>| {
        let pid = pid_apply.clone();
        busy.set(true);
        spawn_local(async move {
            match state_service::apply_state_migration(pid.clone(), target, policy).await {
                Ok(report) => {
                    match &report.outcome {
                        MigrationOutcome::NothingToDo => {
                            toast::info("Nothing needed moving".to_string())
                        }
                        MigrationOutcome::Migrated { files, bytes } => toast::success(format!(
                            "Moved {files} file(s), {} to {}",
                            human_bytes(*bytes),
                            report.to
                        )),
                        MigrationOutcome::KeptDestination { archived_source } => {
                            let extra = archived_source
                                .as_deref()
                                .map(|p| format!(" Previous copy archived at {p}."))
                                .unwrap_or_default();
                            toast::info(format!("Kept the state already at {}.{extra}", report.to))
                        }
                    }
                    for warning in &report.warnings {
                        toast::info(warning.clone());
                    }
                    pending.set(None);
                    match state_service::get_state_location(pid).await {
                        Ok(updated) => info.set(Some(updated)),
                        Err(e) => toast::error(e),
                    }
                }
                Err(e) => toast::error(e),
            }
            busy.set(false);
        });
    };

    let apply_confirm = apply.clone();
    let apply_source = apply.clone();
    let apply_destination = apply;

    view! {
        <div class="space-y-6">
            <div>
                <h2 class="text-lg font-semibold text-white/90 mb-1">"Project storage"</h2>
                <p class="text-sm text-white/50 mb-4">
                    "Choose where SlashIt keeps this project's board. Your API keys, terminal history and logs are always stored outside the project and are never affected by this setting."
                </p>
            </div>

            {move || {
                if loading.get() {
                    return view! {
                        <p class="text-sm text-white/40">"Loading…"</p>
                    }.into_any();
                }

                let Some(current) = info.get() else {
                    return view! {
                        <div class="p-4 rounded-xl bg-white/5 border border-white/10">
                            <p class="text-sm text-white/60">
                                "Select a project to configure its storage."
                            </p>
                        </div>
                    }.into_any();
                };

                if !current.can_choose {
                    return view! {
                        <div class="p-4 rounded-xl bg-amber-500/10 border border-amber-500/20">
                            <p class="text-sm text-amber-200/90">
                                "This project has no repository folder attached, so there is nowhere to store state inside it. Its board is kept outside the project."
                            </p>
                        </div>
                    }.into_any();
                }

                let selected = current.location;
                let current_dir = current.current_dir.clone();

                view! {
                    <div class="space-y-3">
                        {OPTIONS.into_iter().map(|option| {
                            let is_selected = selected == option;
                            let choose = choose.clone();
                            let path_hint = match option {
                                StateLocation::External => current.external_dir.clone(),
                                StateLocation::InProject => current.in_project_dir.clone(),
                                StateLocation::Auto => current.current_dir.clone(),
                            };
                            view! {
                                <button
                                    disabled=move || busy.get()
                                    on:click=move |_| { if !is_selected { choose(option) } }
                                    class=format!(
                                        "w-full text-left p-4 rounded-xl border transition-all disabled:opacity-50 {}",
                                        if is_selected {
                                            "bg-blue-500/15 border-blue-500/40"
                                        } else {
                                            "bg-white/5 border-white/10 hover:border-white/20"
                                        }
                                    )
                                >
                                    <div class="flex items-start gap-3">
                                        <span class=format!(
                                            "mt-1 w-4 h-4 shrink-0 rounded-full border-2 {}",
                                            if is_selected { "border-blue-400 bg-blue-400" } else { "border-white/30" }
                                        )></span>
                                        <div class="min-w-0">
                                            <p class="font-medium text-white/90">{option.label()}</p>
                                            <p class="text-sm text-white/40 mt-1">{option.description()}</p>
                                            <p class="text-xs text-white/30 mt-2 font-mono break-all">{path_hint}</p>
                                        </div>
                                    </div>
                                </button>
                            }
                        }).collect::<Vec<_>>()}

                        <div class="p-3 rounded-lg bg-white/[0.03] border border-white/10">
                            <p class="text-xs text-white/40">"Currently stored at"</p>
                            <p class="text-xs text-white/70 font-mono break-all mt-1">{current_dir}</p>
                        </div>
                    </div>
                }.into_any()
            }}

            {move || {
                let Some((target, plan)) = pending.get() else {
                    return ().into_any();
                };
                let apply_confirm = apply_confirm.clone();
                let apply_source = apply_source.clone();
                let apply_destination = apply_destination.clone();
                let conflicts = plan.conflicts.clone();
                let needs_resolution = plan.requires_resolution;

                view! {
                    <div class="fixed inset-0 z-[70] bg-black/60 flex items-center justify-center p-6">
                        <div class="w-full max-w-lg rounded-xl border border-white/10 bg-[#12141a] p-6 space-y-4">
                            <h3 class="text-lg font-semibold text-white/90">"Move project state"</h3>

                            <div class="space-y-2 text-sm">
                                <div>
                                    <p class="text-white/40 text-xs">"From"</p>
                                    <p class="text-white/80 font-mono break-all">{plan.from.clone()}</p>
                                </div>
                                <div>
                                    <p class="text-white/40 text-xs">"To"</p>
                                    <p class="text-white/80 font-mono break-all">{plan.to.clone()}</p>
                                </div>
                                <p class="text-white/60">
                                    {format!(
                                        "{} file(s), {}",
                                        plan.source_file_count,
                                        human_bytes(plan.source_bytes),
                                    )}
                                </p>
                            </div>

                            {(!conflicts.is_empty()).then(|| view! {
                                <div class="p-3 rounded-lg bg-amber-500/10 border border-amber-500/20 space-y-2">
                                    <p class="text-sm text-amber-200/90">
                                        "Both locations already contain state. Choose which copy to keep — the other is archived beside itself, never deleted."
                                    </p>
                                    <ul class="text-xs text-amber-100/70 font-mono space-y-0.5 max-h-32 overflow-y-auto">
                                        {conflicts.iter().take(20).map(|c| view! { <li>{c.clone()}</li> }).collect::<Vec<_>>()}
                                    </ul>
                                </div>
                            })}

                            <div class="flex flex-wrap justify-end gap-2 pt-2">
                                <button
                                    disabled=move || busy.get()
                                    on:click=move |_| pending.set(None)
                                    class="px-4 py-2 rounded-lg text-white/60 hover:text-white/90 hover:bg-white/5 disabled:opacity-50"
                                >
                                    "Cancel"
                                </button>

                                {if needs_resolution {
                                    view! {
                                        <>
                                            <button
                                                disabled=move || busy.get()
                                                on:click=move |_| apply_destination(target, Some(ConflictPolicy::PreferDestination))
                                                class="px-4 py-2 rounded-lg bg-white/10 text-white/90 hover:bg-white/15 disabled:opacity-50"
                                            >
                                                "Keep the destination copy"
                                            </button>
                                            <button
                                                disabled=move || busy.get()
                                                on:click=move |_| apply_source(target, Some(ConflictPolicy::PreferSource))
                                                class="px-4 py-2 rounded-lg bg-blue-500 text-white hover:bg-blue-400 disabled:opacity-50"
                                            >
                                                "Keep the source copy"
                                            </button>
                                        </>
                                    }.into_any()
                                } else {
                                    view! {
                                        <button
                                            disabled=move || busy.get()
                                            on:click=move |_| apply_confirm(target, None)
                                            class="px-4 py-2 rounded-lg bg-blue-500 text-white hover:bg-blue-400 disabled:opacity-50"
                                        >
                                            {move || if busy.get() { "Moving…" } else { "Move state" }}
                                        </button>
                                    }.into_any()
                                }}
                            </div>
                        </div>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
