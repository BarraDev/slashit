use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::models::{Task, TaskStatus};
use crate::services::list_tasks;

#[component]
pub fn Insights(
    #[prop(default = String::new())] project_id: String,
) -> impl IntoView {
    let (tasks, set_tasks) = signal(Vec::<Task>::new());
    let (loading, set_loading) = signal(true);

    // Load tasks for analytics
    {
        let project_id = project_id.clone();
        Effect::new(move |_| {
            let pid = project_id.clone();
            if pid.is_empty() {
                set_loading.set(false);
                return;
            }
            spawn_local(async move {
                if let Ok(t) = list_tasks(pid).await { set_tasks.set(t) }
                set_loading.set(false);
            });
        });
    }

    let total_tasks = move || tasks.get().len();
    let backlog_count = move || tasks.get().iter().filter(|t| t.status == TaskStatus::Backlog).count();
    let queue_count = move || tasks.get().iter().filter(|t| t.status == TaskStatus::Queue).count();
    let in_progress_count = move || tasks.get().iter().filter(|t| t.status == TaskStatus::InProgress).count();
    let done_count = move || tasks.get().iter().filter(|t| t.status == TaskStatus::Done).count();
    let error_count = move || tasks.get().iter().filter(|t| t.status == TaskStatus::Error).count();

    let completion_rate = move || {
        let total = total_tasks();
        if total == 0 { return 0.0; }
        (done_count() as f64 / total as f64 * 100.0).round()
    };

    let stuck_count = move || tasks.get().iter().filter(|t| t.stuck_since.is_some()).count();

    view! {
        <div class="space-y-6">
            <div>
                <h1 class="text-2xl font-bold text-white/90">"Project Insights"</h1>
                <p class="text-sm text-white/40 mt-1">"Task analytics and project health metrics"</p>
            </div>

            <Show when=move || loading.get()>
                <div class="flex items-center justify-center py-20">
                    <div class="text-white/40">"Loading analytics..."</div>
                </div>
            </Show>

            <Show when=move || !loading.get()>
                // Summary cards
                <div class="grid grid-cols-2 md:grid-cols-4 gap-4">
                    <div class="border border-white/10 rounded-xl bg-white/[0.02] p-5">
                        <div class="text-3xl font-bold text-white/90">{total_tasks}</div>
                        <div class="text-sm text-white/40 mt-1">"Total Tasks"</div>
                    </div>
                    <div class="border border-emerald-500/20 rounded-xl bg-emerald-500/5 p-5">
                        <div class="text-3xl font-bold text-emerald-400">{done_count}</div>
                        <div class="text-sm text-white/40 mt-1">"Completed"</div>
                    </div>
                    <div class="border border-blue-500/20 rounded-xl bg-blue-500/5 p-5">
                        <div class="text-3xl font-bold text-blue-400">{in_progress_count}</div>
                        <div class="text-sm text-white/40 mt-1">"In Progress"</div>
                    </div>
                    <div class="border border-yellow-500/20 rounded-xl bg-yellow-500/5 p-5">
                        <div class="text-3xl font-bold text-yellow-400">
                            {move || format!("{}%", completion_rate())}
                        </div>
                        <div class="text-sm text-white/40 mt-1">"Completion Rate"</div>
                    </div>
                </div>

                // Status breakdown
                <div class="border border-white/10 rounded-xl bg-white/[0.02] p-6">
                    <h3 class="text-lg font-semibold text-white/90 mb-4">"Status Breakdown"</h3>
                    <div class="space-y-3">
                        {move || {
                            let total = total_tasks().max(1) as f64;
                            let statuses = vec![
                                ("Backlog", backlog_count(), "bg-slate-500", "text-slate-300"),
                                ("Queue", queue_count(), "bg-cyan-500", "text-cyan-300"),
                                ("In Progress", in_progress_count(), "bg-blue-500", "text-blue-300"),
                                ("Done", done_count(), "bg-emerald-500", "text-emerald-300"),
                                ("Error", error_count(), "bg-red-500", "text-red-300"),
                            ];
                            statuses.into_iter().map(|(label, count, bar_color, text_color)| {
                                let pct = (count as f64 / total * 100.0).round();
                                view! {
                                    <div class="flex items-center gap-3">
                                        <div class=format!("w-24 text-sm {}", text_color)>{label}</div>
                                        <div class="flex-1 h-6 bg-white/5 rounded-full overflow-hidden">
                                            <div
                                                class=format!("{} h-full rounded-full transition-all duration-500", bar_color)
                                                style=format!("width: {}%", pct)
                                            ></div>
                                        </div>
                                        <div class="w-16 text-right text-sm text-white/60">
                                            {format!("{} ({}%)", count, pct)}
                                        </div>
                                    </div>
                                }
                            }).collect::<Vec<_>>()
                        }}
                    </div>
                </div>

                // Health indicators
                <div class="grid grid-cols-1 md:grid-cols-2 gap-4">
                    <div class="border border-white/10 rounded-xl bg-white/[0.02] p-5">
                        <h3 class="text-sm font-medium text-white/70 mb-3">"Health Indicators"</h3>
                        <div class="space-y-3">
                            <div class="flex items-center justify-between">
                                <span class="text-sm text-white/60">"Stuck Tasks"</span>
                                <span class=move || format!("text-sm font-medium {}", if stuck_count() > 0 { "text-red-400" } else { "text-emerald-400" })>
                                    {stuck_count}
                                </span>
                            </div>
                            <div class="flex items-center justify-between">
                                <span class="text-sm text-white/60">"Error Rate"</span>
                                <span class=move || format!("text-sm font-medium {}", if error_count() > 0 { "text-amber-400" } else { "text-emerald-400" })>
                                    {move || {
                                        let total = total_tasks().max(1) as f64;
                                        format!("{}%", (error_count() as f64 / total * 100.0).round())
                                    }}
                                </span>
                            </div>
                            <div class="flex items-center justify-between">
                                <span class="text-sm text-white/60">"Queue Depth"</span>
                                <span class="text-sm font-medium text-white/80">{queue_count}</span>
                            </div>
                        </div>
                    </div>

                    <div class="border border-white/10 rounded-xl bg-white/[0.02] p-5">
                        <h3 class="text-sm font-medium text-white/70 mb-3">"Task Distribution"</h3>
                        <div class="space-y-3">
                            {move || {
                                let all_tasks = tasks.get();
                                let mut category_counts = std::collections::HashMap::new();
                                for t in &all_tasks {
                                    *category_counts.entry(format!("{}", t.category)).or_insert(0u32) += 1;
                                }
                                let mut sorted: Vec<_> = category_counts.into_iter().collect();
                                sorted.sort_by(|a, b| b.1.cmp(&a.1));
                                sorted.into_iter().take(5).map(|(cat, count)| {
                                    view! {
                                        <div class="flex items-center justify-between">
                                            <span class="text-sm text-white/60">{cat}</span>
                                            <span class="text-sm font-medium text-white/80">{count}</span>
                                        </div>
                                    }
                                }).collect::<Vec<_>>()
                            }}
                        </div>
                    </div>
                </div>
            </Show>
        </div>
    }
}
