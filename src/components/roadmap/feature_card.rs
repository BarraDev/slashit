use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

#[component]
pub fn FeatureCard(
    feature: crate::models::RoadmapFeature,
    on_convert: Callback<Uuid>,
    on_edit: Callback<Uuid>,
    on_delete: Callback<Uuid>,
) -> impl IntoView {
    let (converting, set_converting) = signal(false);
    let feature_id = feature.id;

    let title = feature.title;
    let description = feature.description;
    let (motivation, _set_motivation) = signal(feature.motivation);
    let audience = feature.audience;
    let priority = feature.priority;
    let complexity = feature.complexity;
    let (competitor_analysis, _set_competitor_analysis) = signal(feature.competitor_analysis);
    let linked_task_count = feature.linked_task_ids.len();
    let has_linked_tasks = !feature.linked_task_ids.is_empty();

    let priority_badge = move || match priority {
        crate::models::TaskPriority::Urgent => ("Urgent", "bg-red-500/20 text-red-300"),
        crate::models::TaskPriority::High => ("High", "bg-orange-500/20 text-orange-300"),
        crate::models::TaskPriority::Medium => ("Medium", "bg-yellow-500/20 text-yellow-300"),
        crate::models::TaskPriority::Low => ("Low", "bg-green-500/20 text-green-300"),
    };

    let complexity_badge = move || match complexity {
        crate::models::TaskComplexity::Minimal => ("Minimal", "bg-green-500/20 text-green-300"),
        crate::models::TaskComplexity::Moderate => ("Moderate", "bg-yellow-500/20 text-yellow-300"),
        crate::models::TaskComplexity::Complex => ("Complex", "bg-orange-500/20 text-orange-300"),
        crate::models::TaskComplexity::Advanced => ("Advanced", "bg-red-500/20 text-red-300"),
    };

    view! {
        <div class="border border-white/10 rounded-xl bg-white/[0.02] hover:border-white/20 transition-all p-4 group">
            <div class="flex items-start justify-between gap-3 mb-3">
                <h3 class="font-semibold text-white/90 flex-1">{title}</h3>
                <div class="flex items-center gap-1 opacity-0 group-hover:opacity-100 transition-opacity">
                    <button
                        on:click=move |_| on_edit.run(feature_id)
                        class="p-1.5 rounded-lg hover:bg-white/10 text-white/40 hover:text-white/60 transition-colors"
                        title="Edit feature"
                    >
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M11 5H6a2 2 0 00-2 2v11a2 2 0 002 2h11a2 2 0 002-2v-5m-1.414-9.414a2 2 0 112.828 2.828L11.828 15H9v-2.828l8.586-8.586z" />
                        </svg>
                    </button>
                    <button
                        on:click=move |_| on_delete.run(feature_id)
                        class="p-1.5 rounded-lg hover:bg-red-500/10 text-white/40 hover:text-red-300 transition-colors"
                        title="Delete feature"
                    >
                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 7l-.867 12.142A2 2 0 0116.138 21H7.862a2 2 0 01-1.995-1.858L5 7m5 4v6m4-6v6m1-10V4a1 1 0 00-1-1h-4a1 1 0 00-1 1v3M4 7h16" />
                        </svg>
                    </button>
                </div>
            </div>

            <p class="text-sm text-white/60 mb-3 line-clamp-2">{description}</p>

            <Show when=move || motivation.get().is_some()>
                <div class="mb-3 p-2 rounded-lg bg-purple-500/10 border border-purple-500/20">
                    <p class="text-xs text-purple-200">
                        <span class="font-medium">"Motivation: "</span>
                        {move || motivation.get().unwrap_or_default()}
                    </p>
                </div>
            </Show>

            <div class="flex flex-wrap gap-2 mb-3">
                <crate::components::roadmap::AudienceBadge audience=audience.clone() />
                <span class=format!(
                    "px-2 py-1 rounded text-xs font-medium {}",
                    priority_badge().1
                )>
                    {priority_badge().0}
                </span>
                <span class=format!(
                    "px-2 py-1 rounded text-xs font-medium {}",
                    complexity_badge().1
                )>
                    {complexity_badge().0}
                </span>
            </div>

            <Show when=move || competitor_analysis.get().is_some()>
                <div class="mb-3 p-2 rounded-lg bg-blue-500/10 border border-blue-500/20">
                    <div class="flex items-center gap-2">
                        <svg class="w-4 h-4 text-blue-300" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 19v-6a2 2 0 00-2-2H5a2 2 0 00-2 2v6a2 2 0 002 2h2a2 2 0 002-2zm0 0V9a2 2 0 012-2h2a2 2 0 012 2v10m-6 0a2 2 0 002 2h2a2 2 0 002-2m0 0V5a2 2 0 012-2h2a2 2 0 012 2v14a2 2 0 01-2 2h-2a2 2 0 01-2-2z" />
                        </svg>
                        <span class="text-xs text-blue-200 font-medium">"Market Opportunity: "</span>
                        <span class="text-xs text-blue-300">
                            {move || match &competitor_analysis.get() {
                                Some(a) => match a.market_opportunity {
                                    crate::models::MarketOpportunity::High => "High",
                                    crate::models::MarketOpportunity::Medium => "Medium",
                                    crate::models::MarketOpportunity::Low => "Low",
                                }
                                None => "Unknown"
                            }}
                        </span>
                    </div>
                </div>
            </Show>

            <Show when=move || has_linked_tasks>
                <div class="mb-3 flex items-center gap-1.5 text-xs text-white/50">
                    <svg class="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4" />
                    </svg>
                    <span>{format!("{} task(s) linked", linked_task_count)}</span>
                </div>
            </Show>

            <div class="flex items-center justify-between pt-3 border-t border-white/5">
                <button
                    disabled=move || converting.get() || has_linked_tasks
                    on:click=move |_| {
                        let feature_id = feature_id;
                        let on_convert = on_convert;
                        set_converting.set(true);
                        spawn_local(async move {
                            let _ = crate::services::convert_to_task(feature_id.to_string()).await;
                            on_convert.run(feature_id);
                            set_converting.set(false);
                        });
                    }
                    class=format!(
                        "px-3 py-1.5 rounded-lg text-sm font-medium bg-blue-500 hover:bg-blue-600 disabled:bg-white/5 disabled:text-white/30 text-white transition-colors flex items-center gap-1.5 {}",
                        if converting.get() { "opacity-75" } else { "" }
                    )
                >
                    {move || if converting.get() {
                        view! {
                            <svg class="w-3.5 h-3.5 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                            </svg>
                        }.into_any()
                    } else {
                        view! {
                            <svg class="w-3.5 h-3.5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                            </svg>
                        }.into_any()
                    }}
                    <span>{move || if converting.get() { "Converting..." } else { "Convert to Task" } }</span>
                </button>

                <div class="flex items-center gap-1.5 text-xs text-white/40">
                    {move || if has_linked_tasks {
                        view! {
                            <svg class="w-3.5 h-3.5 text-green-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12l2 2 4-4m6 2a9 9 0 11-18 0 9 9 0 0118 0z" />
                            </svg>
                            <span>"Converted"</span>
                        }.into_any()
                    } else {
                        ().into_any()
                    }}
                </div>
            </div>
        </div>
    }
}
