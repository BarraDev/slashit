use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

#[component]
pub fn Roadmap(project_id: String) -> impl IntoView {
    let (features, set_features) = signal(Vec::<crate::models::RoadmapFeature>::new());
    let (loading, set_loading) = signal(false);
    let (error_msg, set_error_msg) = signal(None::<String>);

    let (show_analysis_modal, set_show_analysis_modal) = signal(false);
    let (analysis_feature_id, set_analysis_feature_id) = signal(Uuid::nil());

    let project_id_clone = project_id.clone();
    let load_features = {
        let project_id = project_id_clone.clone();
        move || {
            let project_id = project_id.clone();

            spawn_local(async move {
                set_loading.set(true);
                set_error_msg.set(None);

                match crate::services::list_roadmap_features(project_id).await {
                    Ok(f) => set_features.set(f),
                    Err(e) => set_error_msg.set(Some(e)),
                }
                set_loading.set(false);
            });
        }
    };

    let on_convert = {
        let load_features = load_features.clone();
        Callback::new(move |_: Uuid| {
            load_features();
        })
    };

    let on_delete = {
        let load_features = load_features.clone();
        Callback::new(move |feature_id: Uuid| {
            let load_features = load_features.clone();
            spawn_local(async move {
                let _ = crate::services::delete_roadmap_feature(feature_id.to_string()).await;
                load_features();
            });
        })
    };

    let on_status_change = {
        let load_features = load_features.clone();
        Callback::new(move |(id, status): (Uuid, crate::models::RoadmapStatus)| {
            let load_features = load_features.clone();
            spawn_local(async move {
                match crate::services::update_roadmap_feature_status(id.to_string(), status).await {
                    Ok(_) => load_features(),
                    Err(e) => set_error_msg.set(Some(format!("Failed to update status: {}", e))),
                }
            });
        })
    };

    let on_analysis_complete = {
        let load_features = load_features.clone();
        Callback::new(move |_: crate::models::CompetitorAnalysis| {
            set_show_analysis_modal.set(false);
            load_features();
        })
    };

    view! {
        <div class="roadmap-page">
            <div class="roadmap-header">
                <h1>"Roadmap"</h1>
            </div>

            {
                move || {
                    if let Some(err) = error_msg.get() {
                        view! {
                            <div class="error_msg-message">{err}</div>
                        }.into_any()
                    } else if loading.get() {
                        view! {
                            <div class="loading">"Loading..."</div>
                        }.into_any()
                    } else {
                        let on_convert = on_convert;
                        let on_delete = on_delete;
                        let on_status_change = on_status_change;
                        view! {
                            <crate::components::roadmap::RoadmapBoard
                                features=features.get()
                                on_convert=on_convert
                                on_edit=Callback::new(move |_: Uuid| {})
                                on_delete=on_delete
                                on_status_change=on_status_change
                            />
                        }.into_any()
                    }
                }
            }

            <crate::components::roadmap::CompetitorAnalysisModal
                show=show_analysis_modal
                set_show=set_show_analysis_modal
                feature_id=Signal::derive(move || analysis_feature_id.get())
                on_complete=on_analysis_complete
            />
        </div>
    }
}
