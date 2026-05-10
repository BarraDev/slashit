use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::services::{get_project_path};
use crate::components::toast;

#[component]
pub fn Spec(
    #[prop(default = String::new())] project_id: String,
) -> impl IntoView {
    let (spec_content, set_spec_content) = signal(String::new());
    let (saving, set_saving) = signal(false);
    let (last_saved, set_last_saved) = signal(Option::<String>::None);
    let (spec_path, set_spec_path) = signal(Option::<String>::None);
    let (loading, set_loading) = signal(true);

    // Load spec on mount when project_id is available
    {
        let project_id = project_id.clone();
        Effect::new(move |_| {
            let pid = project_id.clone();
            if pid.is_empty() {
                set_loading.set(false);
                return;
            }

            spawn_local(async move {
                // Get project path
                if let Ok(Some(path)) = get_project_path(pid).await {
                    let file_path = format!("{}/spec.md", path);
                    set_spec_path.set(Some(file_path.clone()));

                    // Try to read existing spec
                    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                        "path": file_path,
                    })).unwrap();

                    // Use invoke directly for read_file
                    #[allow(unused_imports)]
                    use wasm_bindgen::prelude::*;
                    #[wasm_bindgen]
                    extern "C" {
                        #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
                        async fn invoke(cmd: &str, args: JsValue) -> JsValue;
                    }

                    let result = invoke("read_file", args).await;
                    if let Ok(content) = serde_wasm_bindgen::from_value::<String>(result) {
                        set_spec_content.set(content);
                    }
                }
                set_loading.set(false);
            });
        });
    }

    let (confirming_clear, set_confirming_clear) = signal(false);

    let on_save = move |_| {
        set_confirming_clear.set(false);
        let content = spec_content.get();
        let path = spec_path.get();

        if content.trim().is_empty() || path.is_none() {
            return;
        }

        let path = path.unwrap();
        set_saving.set(true);

        spawn_local(async move {
            #[wasm_bindgen::prelude::wasm_bindgen]
            extern "C" {
                #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
                async fn invoke(cmd: &str, args: wasm_bindgen::JsValue) -> wasm_bindgen::JsValue;
            }

            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "path": path,
                "content": content,
            })).unwrap();

            let result = invoke("write_file", args).await;
            match serde_wasm_bindgen::from_value::<()>(result) {
                Ok(()) => {
                    set_last_saved.set(Some("Just now".to_string()));
                    toast::success("Spec saved".to_string());
                }
                Err(e) => {
                    toast::error(format!("Failed to save: {}", e));
                }
            }
            set_saving.set(false);
        });
    };

    let on_clear = move |_| {
        if confirming_clear.get() {
            set_spec_content.set(String::new());
            set_last_saved.set(None);
            set_confirming_clear.set(false);
        } else {
            set_confirming_clear.set(true);
            // Auto-reset after 3 seconds
            let handle = gloo_timers::callback::Timeout::new(3_000, move || {
                set_confirming_clear.set(false);
            });
            handle.forget();
        }
    };

    let char_count = move || spec_content.get().chars().count();
    let word_count = move || spec_content.get().split_whitespace().filter(|w| !w.is_empty()).count();
    let line_count = move || spec_content.get().lines().count();

    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"Project Specification"</h1>
                    <p class="text-sm text-white/40 mt-1">
                        {move || if let Some(p) = spec_path.get() {
                            format!("Editing: {}", p)
                        } else {
                            "Select a project to edit its specification".to_string()
                        }}
                    </p>
                </div>
                <div class="flex items-center gap-3">
                    <button
                        on:click=on_clear
                        class=move || format!(
                            "px-4 py-2 rounded-xl transition-all border {}",
                            if confirming_clear.get() {
                                "bg-red-500/20 hover:bg-red-500/30 text-red-300 border-red-500/30"
                            } else {
                                "bg-white/5 hover:bg-white/10 text-white/70 hover:text-white/90 border-white/10"
                            }
                        )
                    >
                        {move || if confirming_clear.get() { "Confirm Clear" } else { "Clear" }}
                    </button>
                    <button
                        on:click=on_save
                        disabled=move || saving.get() || spec_content.get().trim().is_empty() || spec_path.get().is_none()
                        class="px-6 py-2 rounded-xl bg-blue-500 hover:bg-blue-600 disabled:bg-white/5 disabled:text-white/30 text-white font-medium transition-colors flex items-center gap-2 disabled:cursor-not-allowed"
                    >
                        {move || if saving.get() {
                            view! {
                                <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                </svg>
                            }.into_any()
                        } else {
                            view! {
                                <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M8 7H5a2 2 0 00-2 2v9a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-3m-1 4l-3 3m0 0l-3-3m3 3V4" />
                                </svg>
                            }.into_any()
                        }}
                        <span>{move || if saving.get() { "Saving..." } else { "Save" } }</span>
                    </button>
                </div>
            </div>

            <Show when=move || loading.get()>
                <div class="flex items-center justify-center py-20">
                    <div class="text-white/40">"Loading specification..."</div>
                </div>
            </Show>

            <Show when=move || !loading.get()>
                <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
                    <div class="px-6 py-4 border-b border-white/10 flex items-center justify-between">
                        <div class="flex items-center gap-4">
                            <div class="flex items-center gap-2">
                                <svg class="w-5 h-5 text-white/40" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                                </svg>
                                <span class="text-sm text-white/60">"spec.md"</span>
                            </div>
                            <div class="flex items-center gap-3 text-xs text-white/40">
                                <span>{move || format!("{} lines", line_count())}</span>
                                <span>"·"</span>
                                <span>{move || format!("{} words", word_count())}</span>
                                <span>"·"</span>
                                <span>{move || format!("{} chars", char_count())}</span>
                            </div>
                        </div>
                        {move || {
                            if let Some(saved) = last_saved.get() {
                                view! {
                                    <div class="flex items-center gap-2 text-xs text-green-400">
                                        <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                            <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                        </svg>
                                        <span>"Saved "</span>
                                        <span>{saved}</span>
                                    </div>
                                }.into_any()
                            } else {
                                ().into_any()
                            }
                        }}
                    </div>

                    <textarea
                        placeholder="# Project Specification\n\nDescribe your project requirements, goals, and technical decisions here.\nThis will be used by AI agents as context for task execution."
                        prop:value=spec_content
                        on:input=move |ev| set_spec_content.set(event_target_value(&ev))
                        class="w-full min-h-[600px] px-6 py-4 bg-black/40 text-white/90 placeholder-white/30 focus:outline-none resize-none font-mono text-sm leading-relaxed"
                    />
                </div>
            </Show>
        </div>
    }
}
