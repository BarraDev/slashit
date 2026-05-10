use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;

#[component]
pub fn CompetitorAnalysisModal(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    #[prop(into)] feature_id: Signal<Uuid>,
    on_complete: Callback<crate::models::CompetitorAnalysis>,
) -> impl IntoView {
    let (competitors, set_competitors) = signal(String::new());
    let (analyzing, set_analyzing) = signal(false);
    let (error, set_error) = signal(None::<String>);
    let (result, set_result) = signal(None::<crate::models::CompetitorAnalysis>);

    let on_analyze = move |_| {
        let feature_id = feature_id.get();
        let set_show = set_show;
        let set_error = set_error;
        let set_result = set_result;
        let set_analyzing = set_analyzing;
        let on_complete = on_complete;

        let competitor_list: Vec<String> = competitors
            .get()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        if competitor_list.is_empty() {
            set_error.set(Some("Please enter at least one competitor".to_string()));
            return;
        }

        set_analyzing.set(true);
        set_error.set(None);

        spawn_local(async move {
            let req = crate::models::CompetitorAnalysisRequest {
                feature_id,
                competitors: competitor_list,
            };

            match crate::services::run_competitor_analysis(req).await {
                Ok(analysis) => {
                    set_result.set(Some(analysis.clone()));
                    on_complete.run(analysis);
                }
                Err(e) => {
                    set_error.set(Some(e));
                }
            }
            set_analyzing.set(false);
        });
    };

    view! {
        <Show when=move || show.get()>
            <div class="modal-overlay" on:click=move |_| set_show.set(false)>
                <div class="modal" on:click=move |e| e.stop_propagation()>
                    <div class="modal-header">
                        <h2>"🔍 Competitor Analysis"</h2>
                        <button class="modal-close" on:click=move |_| set_show.set(false)>"×"</button>
                    </div>

                    <div class="modal-body">
                        <Show when=move || result.get().is_none()>
                            <div class="form-group">
                                <label for="competitors">"Competitors (comma-separated)"</label>
                                <input
                                    id="competitors"
                                    type="text"
                                    placeholder="e.g. GitHub, GitLab, Bitbucket"
                                    prop:value=move || competitors.get()
                                    on:input=move |ev| {
                                        set_competitors.set(event_target_value(&ev));
                                    }
                                />
                                <small>"Enter competitor names separated by commas"</small>
                            </div>

                            {
                                move || {
                                    if let Some(err) = error.get() {
                                        view! {
                                            <div class="error-message">{err}</div>
                                        }.into_any()
                                    } else {
                                        ().into_any()
                                    }
                                }
                            }
                        </Show>

                        <Show when=move || result.get().is_some()>
                            {move || {
                                result.get().map(|analysis| {
                                    view! {
                                        <div class="analysis-result">
                                            <h3>"Analysis Results"</h3>

                                            <div class="gap-analysis">
                                                <h4>"Gap Analysis"</h4>
                                                <p>{analysis.gap_analysis}</p>
                                            </div>

                                            <div class="market-opportunity">
                                                <h4>"Market Opportunity"</h4>
                                                <span class="opportunity-badge" class:high=matches!(analysis.market_opportunity, crate::models::MarketOpportunity::High)>
                                                    {match analysis.market_opportunity {
                                                        crate::models::MarketOpportunity::High => "🔥 High Opportunity",
                                                        crate::models::MarketOpportunity::Medium => "⚖️ Medium Opportunity",
                                                        crate::models::MarketOpportunity::Low => "📉 Low Opportunity",
                                                    }}
                                                </span>
                                            </div>

                                            <div class="competitor-features">
                                                <h4>"Competitor Features"</h4>
                                                <For
                                                    each=move || analysis.competitors.clone()
                                                    key=|c| format!("{}-{}", c.competitor_name, c.feature_name)
                                                    children=move |comp| {
                                                        view! {
                                                            <div class="competitor-item">
                                                                <span class="competitor-name">{comp.competitor_name}</span>
                                                                <span class="has-feature" class:yes=comp.has_feature class:no=!comp.has_feature>
                                                                    {if comp.has_feature { "✅ Has" } else { "❌ Missing" } }
                                                                </span>
                                                            </div>
                                                        }
                                                    }
                                                />
                                            </div>
                                        </div>
                                    }
                                })
                            }}
                        </Show>
                    </div>

                    <div class="modal-footer">
                        <button class="btn-secondary" on:click=move |_| set_show.set(false) disabled=move || analyzing.get()>
                            "Close"
                        </button>
                        <Show when=move || result.get().is_none()>
                            <button
                                class="btn-primary"
                                on:click=on_analyze
                                disabled=move || analyzing.get()
                            >
                                {move || if analyzing.get() { "Analyzing..." } else { "Run Analysis" } }
                            </button>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}
