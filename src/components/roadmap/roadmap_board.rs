use leptos::prelude::*;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq)]
pub enum RoadmapColumn {
    Proposed,
    Planned,
    InProgress,
    Completed,
}

impl RoadmapColumn {
    fn title(&self) -> &'static str {
        match self {
            RoadmapColumn::Proposed => "Proposed",
            RoadmapColumn::Planned => "Planned",
            RoadmapColumn::InProgress => "In Progress",
            RoadmapColumn::Completed => "Completed",
        }
    }

    fn status(&self) -> crate::models::RoadmapStatus {
        match self {
            RoadmapColumn::Proposed => crate::models::RoadmapStatus::Proposed,
            RoadmapColumn::Planned => crate::models::RoadmapStatus::Planned,
            RoadmapColumn::InProgress => crate::models::RoadmapStatus::InProgress,
            RoadmapColumn::Completed => crate::models::RoadmapStatus::Completed,
        }
    }

    fn color_class(&self) -> &'static str {
        match self {
            RoadmapColumn::Proposed => "border-purple-500/30 bg-purple-500/5",
            RoadmapColumn::Planned => "border-blue-500/30 bg-blue-500/5",
            RoadmapColumn::InProgress => "border-yellow-500/30 bg-yellow-500/5",
            RoadmapColumn::Completed => "border-green-500/30 bg-green-500/5",
        }
    }

    fn text_class(&self) -> &'static str {
        match self {
            RoadmapColumn::Proposed => "text-purple-300",
            RoadmapColumn::Planned => "text-blue-300",
            RoadmapColumn::InProgress => "text-yellow-300",
            RoadmapColumn::Completed => "text-green-300",
        }
    }
}

#[component]
pub fn RoadmapBoard(
    features: Vec<crate::models::RoadmapFeature>,
    on_convert: Callback<Uuid>,
    on_edit: Callback<Uuid>,
    on_delete: Callback<Uuid>,
    on_status_change: Callback<(Uuid, crate::models::RoadmapStatus)>,
) -> impl IntoView {
    let columns = [
        RoadmapColumn::Proposed,
        RoadmapColumn::Planned,
        RoadmapColumn::InProgress,
        RoadmapColumn::Completed,
    ];

    view! {
        <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-4 gap-6">
            {columns.into_iter().map(|column| {
                view! {
                    <RoadmapColumnView
                        title=column.title().to_string()
                        status=column.status()
                        color_class=column.color_class()
                        text_class=column.text_class()
                        features=features.clone()
                        on_convert=on_convert
                        on_edit=on_edit
                        on_delete=on_delete
                        on_status_change=on_status_change
                    />
                }
            }).collect::<Vec<_>>()}
        </div>
    }
}

#[component]
fn RoadmapColumnView(
    title: String,
    status: crate::models::RoadmapStatus,
    color_class: &'static str,
    text_class: &'static str,
    features: Vec<crate::models::RoadmapFeature>,
    on_convert: Callback<Uuid>,
    on_edit: Callback<Uuid>,
    on_delete: Callback<Uuid>,
    on_status_change: Callback<(Uuid, crate::models::RoadmapStatus)>,
) -> impl IntoView {
    let column_features = move || {
        features
            .iter()
            .filter(|f| f.status == status)
            .cloned()
            .collect::<Vec<_>>()
    };

    let column_features_clone = column_features.clone();
    let feature_count = move || column_features_clone().len();

    view! {
        <div class=format!(
            "border rounded-xl p-4 {}",
            color_class
        )>
            <div class="flex items-center justify-between mb-4">
                <h3 class="font-semibold text-white/90">{title}</h3>
                <span class=format!(
                    "px-2 py-1 rounded-lg text-sm font-medium {}",
                    text_class
                )>
                    {feature_count}
                </span>
            </div>

            <div class="space-y-3 min-h-[200px]">
                {move || {
                    let features = column_features();
                    if features.is_empty() {
                        view! {
                            <div class="flex items-center justify-center py-8 text-white/20">
                                <div class="text-center">
                                    <svg class="w-10 h-10 mx-auto mb-2 opacity-30" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2" />
                                    </svg>
                                    <p class="text-sm">"No features"</p>
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <div class="space-y-3">
                                {features.into_iter().map(|feature| {
                                    view! {
                                        <crate::components::roadmap::FeatureCard
                                            feature=feature
                                            on_convert=on_convert
                                            on_edit=on_edit
                                            on_delete=on_delete
                                        />
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
