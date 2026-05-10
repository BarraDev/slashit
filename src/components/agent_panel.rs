use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::models::{AgentExecution, AgentStatus};
use crate::services::{start_agent, stop_agent, get_agent_status};
use crate::components::icons::*;

#[component]
pub fn AgentPanel(#[prop(into)] workspace_id: String) -> impl IntoView {
    let (execution, set_execution) = signal(None::<AgentExecution>);
    let (status, set_status) = signal(None::<AgentStatus>);
    let (logs, set_logs) = signal(Vec::<String>::new());

    let workspace_id_clone = workspace_id.clone();
    let is_running = move || status.get().map(|s| s == AgentStatus::Running).unwrap_or(false);

    let start = move || {
        let workspace_id = workspace_id_clone.clone();
        let set_execution = set_execution;
        let set_status = set_status;

        spawn_local(async move {
            match start_agent(workspace_id, None).await {
                Ok(exec) => {
                    set_status.set(Some(exec.status.clone()));
                    set_execution.set(Some(exec));
                }
                Err(e) => eprintln!("Failed to start agent: {}", e),
            }
        });
    };

    let stop = move || {
        if let Some(exec) = execution.get_untracked() {
            let execution_id = exec.id.to_string();
            let set_status = set_status;
            let set_execution = set_execution;

            spawn_local(async move {
                match stop_agent(execution_id).await {
                    Ok(_) => {
                        set_status.set(Some(AgentStatus::Stopped));
                        set_execution.set(None);
                    }
                    Err(e) => eprintln!("Failed to stop agent: {}", e),
                }
            });
        }
    };

    let status_badge = move || {
        match status.get() {
            None => ("Idle", "bg-white/5 text-white/40", ""),
            Some(AgentStatus::Starting) => ("Starting...", "bg-yellow-500/20 text-yellow-300", ""),
            Some(AgentStatus::Running) => ("Running", "bg-green-500/20 text-green-300 animate-pulse", "ring-2 ring-green-500/30"),
            Some(AgentStatus::Stopping) => ("Stopping...", "bg-orange-500/20 text-orange-300", ""),
            Some(AgentStatus::Stopped) => ("Stopped", "bg-gray-500/20 text-gray-300", ""),
            Some(AgentStatus::Failed(_)) => ("Failed", "bg-red-500/20 text-red-300", "ring-2 ring-red-500/30"),
        }
    };

    view! {
        <div class=format!(
            "border rounded-xl overflow-hidden transition-all duration-200 {}",
            if is_running() {
                "border-green-500/50 bg-green-500/5"
            } else {
                "border-white/10 bg-white/[0.02]"
            }
        )>
            <div class="p-4 border-b border-white/5">
                <div class="flex items-center justify-between">
                    <div class="flex items-center gap-3">
                        <TerminalIcon class=format!(
                            "w-5 h-5 {}",
                            if is_running() { "text-green-400" } else { "text-white/40" }
                        ) />
                        <div>
                            <h3 class="font-semibold text-white/90">"Agent Terminal"</h3>
                            <p class="text-xs text-white/40">{workspace_id.clone()}</p>
                        </div>
                    </div>

                    <div class="flex items-center gap-2">
                        <span class=format!(
                            "px-3 py-1.5 rounded-lg text-sm font-medium {}",
                            status_badge().1
                        )>
                            {status_badge().0}
                        </span>
                    </div>
                </div>

                <div class="flex items-center gap-2 mt-4">
                    <button
                        disabled=move || status.get().map(|s| s == AgentStatus::Running).unwrap_or(false)
                        on:click=move |_| start()
                        class="flex-1 flex items-center justify-center gap-2 px-4 py-2 rounded-lg bg-blue-500 hover:bg-blue-600 disabled:bg-white/5 disabled:text-white/30 text-white transition-colors"
                        aria-label="Start agent"
                        type="button"
                    >
                        <PlayIcon class="w-4 h-4" />
                        <span>"Start"</span>
                    </button>
                    <button
                        disabled=move || !is_running()
                        on:click=move |_| stop()
                        class="flex-1 flex items-center justify-center gap-2 px-4 py-2 rounded-lg bg-red-500 hover:bg-red-600 disabled:bg-white/5 disabled:text-white/30 text-white transition-colors"
                        aria-label="Stop agent"
                        type="button"
                    >
                        <StopIcon class="w-4 h-4" />
                        <span>"Stop"</span>
                    </button>
                    <button
                        disabled=move || logs.get().is_empty()
                        on:click=move |_| {
                            set_logs.set(Vec::new());
                        }
                        class="px-3 py-2 rounded-lg bg-white/5 hover:bg-white/10 disabled:bg-white/5 disabled:text-white/20 text-white/60 transition-colors"
                        title="Clear logs"
                        aria-label="Clear logs"
                        type="button"
                    >
                        <TrashIcon class="w-4 h-4" />
                    </button>
                </div>
            </div>

            <div class="p-4 bg-black/40 min-h-[200px] max-h-[400px] overflow-y-auto font-mono text-sm">
                {move || {
                    let current_logs = logs.get();
                    if current_logs.is_empty() {
                        view! {
                            <div class="flex items-center justify-center h-full text-white/20">
                                <p class="text-sm">"No output yet. Start the agent to see logs."</p>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="space-y-1">
                                {current_logs.into_iter().map(|log| {
                                    view! {
                                        <div class="text-white/60 hover:text-white/80 transition-colors">
                                            <span class="text-white/30">"› "</span>
                                            {log}
                                        </div>
                                    }
                                }).collect::<Vec<_>>()}
                            </div>
                        }.into_any()
                    }
                }}
            </div>
        </div>
    }
}
