use leptos::prelude::*;
use crate::models::AgentLogEntry;

#[component]
pub fn LogViewer(logs: Vec<AgentLogEntry>) -> impl IntoView {
    let (auto_scroll, set_auto_scroll) = signal(true);
    let (filter_level, set_filter_level) = signal::<Option<crate::models::LogLevel>>(None);

    let filtered_logs = move || {
        let level_filter = filter_level.get();
        if let Some(level) = level_filter {
            logs.iter()
                .filter(|log| {
                    matches!(
                        (&log.level, &level),
                        (crate::models::LogLevel::Debug, crate::models::LogLevel::Debug) |
                        (crate::models::LogLevel::Info, crate::models::LogLevel::Info) |
                        (crate::models::LogLevel::Warn, crate::models::LogLevel::Warn) |
                        (crate::models::LogLevel::Error, crate::models::LogLevel::Error)
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        } else {
            logs.clone()
        }
    };

    let level_badge = |level: crate::models::LogLevel| -> (&'static str, &'static str) {
        match level {
            crate::models::LogLevel::Debug => ("DEBUG", "bg-purple-500/20 text-purple-300"),
            crate::models::LogLevel::Info => ("INFO", "bg-blue-500/20 text-blue-300"),
            crate::models::LogLevel::Warn => ("WARN", "bg-yellow-500/20 text-yellow-300"),
            crate::models::LogLevel::Error => ("ERROR", "bg-red-500/20 text-red-300"),
        }
    };

    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
            <div class="flex items-center justify-between px-4 py-3 border-b border-white/5">
                <div class="flex items-center gap-2">
                    <svg class="w-5 h-5 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                    </svg>
                    <h3 class="font-semibold text-white/90">"Agent Logs"</h3>
                    <span class=format!(
                        "px-2 py-0.5 rounded text-xs font-medium bg-white/5 text-white/40",
                    )>
                        {filtered_logs().len()}
                    </span>
                </div>

                <div class="flex items-center gap-3">
                    <div class="flex items-center gap-1.5">
                        <button
                            on:click=move |_| set_filter_level.set(None)
                            class=format!(
                                "px-2.5 py-1 rounded text-xs font-medium transition-all {}",
                                if filter_level.get().is_none() {
                                    "bg-white/10 text-white/90"
                                } else {
                                    "text-white/40 hover:text-white/60 hover:bg-white/5"
                                }
                            )
                        >
                            "All"
                        </button>
                        <button
                            on:click=move |_| set_filter_level.set(Some(crate::models::LogLevel::Error))
                            class=format!(
                                "px-2.5 py-1 rounded text-xs font-medium transition-all {}",
                                if matches!(filter_level.get(), Some(crate::models::LogLevel::Error)) {
                                    "bg-red-500/20 text-red-300"
                                } else {
                                    "text-white/40 hover:text-white/60 hover:bg-white/5"
                                }
                            )
                        >
                            "Errors"
                        </button>
                    </div>

                    <label class="flex items-center gap-2 text-sm text-white/60 cursor-pointer">
                        <input
                            type="checkbox"
                            prop:checked=auto_scroll
                            on:change=move |ev| {
                                set_auto_scroll.set(event_target_checked(&ev));
                            }
                            class="w-4 h-4 rounded border-white/20 bg-white/5 text-blue-500 focus:ring-2 focus:ring-blue-500/50 focus:ring-offset-0 focus:offset-0"
                        />
                        "Auto-scroll"
                    </label>
                </div>
            </div>

            <div class="bg-black/40 p-4 min-h-[300px] max-h-[500px] overflow-y-auto font-mono text-sm">
                {move || {
                    let current_logs = filtered_logs();
                    if current_logs.is_empty() {
                        view! {
                            <div class="flex items-center justify-center h-full min-h-[200px] text-white/20">
                                <div class="text-center">
                                    <svg class="w-12 h-12 mx-auto mb-3 opacity-30" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                                    </svg>
                                    <p class="text-sm">"No logs available"</p>
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="space-y-2">
                                {current_logs.into_iter().map(|log| {
                                    let (label, badge_class) = level_badge(log.level);
                                    view! {
                                        <div class="flex items-start gap-3 p-2 rounded-lg hover:bg-white/5 transition-colors group">
                                            <span class="text-white/30 text-xs font-mono shrink-0 mt-0.5">
                                                {log.timestamp.format("%H:%M:%S%.3f").to_string()}
                                            </span>
                                            <span class=format!(
                                                "px-1.5 py-0.5 rounded text-xs font-mono font-medium shrink-0 {}",
                                                badge_class
                                            )>
                                                {label}
                                            </span>
                                            <span class="flex-1 text-white/70 group-hover:text-white/90 transition-colors break-words">
                                                {log.message}
                                            </span>
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
