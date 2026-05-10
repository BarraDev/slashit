use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::services::get_project_path;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct FileItem {
    name: String,
    path: String,
    is_dir: bool,
    size: Option<u64>,
}

#[component]
pub fn Context(
    #[prop(default = String::new())] project_id: String,
) -> impl IntoView {
    let (files, set_files) = signal(Vec::<FileItem>::new());
    let (all_files, set_all_files) = signal(Vec::<FileItem>::new());
    let (loading, set_loading) = signal(true);
    let (project_path, set_project_path) = signal(Option::<String>::None);
    let (selected_file, set_selected_file) = signal(Option::<String>::None);
    let (file_content, set_file_content) = signal(String::new());
    let (show_all_files, set_show_all_files) = signal(false);

    // Load files from project directory
    {
        let project_id = project_id.clone();
        Effect::new(move |_| {
            let pid = project_id.clone();
            if pid.is_empty() {
                set_loading.set(false);
                return;
            }
            set_loading.set(true);
            spawn_local(async move {
                if let Ok(Some(path)) = get_project_path(pid).await {
                    set_project_path.set(Some(path.clone()));

                    // List files in project root
                    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                        "path": path,
                    })).unwrap();
                    let result = invoke("list_files", args).await;
                    if let Ok(items) = serde_wasm_bindgen::from_value::<Vec<FileItem>>(result) {
                        set_all_files.set(items.clone());
                        // Filter to show relevant context files
                        let context_files: Vec<FileItem> = items.into_iter()
                            .filter(|f| {
                                let name = f.name.to_lowercase();
                                name.ends_with(".md") || name.ends_with(".txt") ||
                                name == "claude.md" || name.contains("claude") ||
                                name == ".env.example" || name == "cargo.toml" ||
                                name == "package.json" || name == "readme.md" ||
                                f.is_dir && (name == "src" || name == ".claude" || name == "docs")
                            })
                            .collect();
                        set_files.set(context_files);
                    }
                }
                set_loading.set(false);
            });
        });
    }

    let on_file_click = move |path: String| {
        let path_clone = path.clone();
        set_selected_file.set(Some(path.clone()));
        spawn_local(async move {
            let args = serde_wasm_bindgen::to_value(&serde_json::json!({
                "path": path_clone,
            })).unwrap();
            let result = invoke("read_file", args).await;
            if let Ok(content) = serde_wasm_bindgen::from_value::<String>(result) {
                set_file_content.set(content);
            } else {
                set_file_content.set("[Could not read file]".to_string());
            }
        });
    };

    fn format_size(size: Option<u64>) -> String {
        match size {
            Some(s) if s < 1024 => format!("{} B", s),
            Some(s) if s < 1024 * 1024 => format!("{:.1} KB", s as f64 / 1024.0),
            Some(s) => format!("{:.1} MB", s as f64 / (1024.0 * 1024.0)),
            None => "—".to_string(),
        }
    }

    view! {
        <div class="space-y-6">
            <div class="flex items-center justify-between">
                <div>
                    <h1 class="text-2xl font-bold text-white/90">"Context Management"</h1>
                    <p class="text-sm text-white/40 mt-1">
                        {move || if let Some(p) = project_path.get() {
                            format!("Project files: {}", p)
                        } else {
                            "Select a project to view context files".to_string()
                        }}
                    </p>
                </div>
                <label class="flex items-center gap-2 cursor-pointer select-none">
                    <span class="text-sm text-white/60">"Show all files"</span>
                    <button
                        on:click=move |_| set_show_all_files.set(!show_all_files.get())
                        class=move || format!(
                            "relative w-11 h-6 rounded-full transition-colors {}",
                            if show_all_files.get() { "bg-yellow-500" } else { "bg-white/10" }
                        )
                    >
                        <span class=move || format!(
                            "absolute top-0.5 left-0.5 w-5 h-5 rounded-full bg-white transition-transform {}",
                            if show_all_files.get() { "translate-x-5" } else { "" }
                        )></span>
                    </button>
                </label>
            </div>

            <Show when=move || loading.get()>
                <div class="flex items-center justify-center py-20">
                    <div class="text-white/40">"Loading project files..."</div>
                </div>
            </Show>

            <Show when=move || !loading.get()>
                <div class="grid grid-cols-1 lg:grid-cols-2 gap-4">
                    // File list
                    <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
                        <div class="px-6 py-4 border-b border-white/10">
                            <div class="flex items-center justify-between">
                                <h2 class="font-semibold text-white/90">"Project Files"</h2>
                                <span class="px-3 py-1 rounded-lg bg-blue-500/20 text-blue-300 text-sm font-medium">
                                    {move || {
                                        let count = if show_all_files.get() { all_files.get().len() } else { files.get().len() };
                                        format!("{} items", count)
                                    }}
                                </span>
                            </div>
                        </div>

                        {move || {
                            let file_list = if show_all_files.get() { all_files.get() } else { files.get() };
                            if file_list.is_empty() {
                                view! {
                                    <div class="p-12 text-center">
                                        <p class="text-white/40">"No context files found in project"</p>
                                    </div>
                                }.into_any()
                            } else {
                                view! {
                                    <div class="divide-y divide-white/5 max-h-[600px] overflow-y-auto">
                                        {file_list.into_iter().map(|file| {
                                            let path = file.path.clone();
                                            let is_dir = file.is_dir;
                                            let size_str = format_size(file.size);
                                            let on_click = on_file_click;
                                            let is_selected = move || selected_file.get().as_deref() == Some(&path);
                                            let path_click = file.path.clone();
                                            view! {
                                                <button
                                                    class=move || format!(
                                                        "w-full px-6 py-3 text-left hover:bg-white/5 transition-colors flex items-center gap-3 {}",
                                                        if is_selected() { "bg-white/5 border-l-2 border-yellow-400" } else { "" }
                                                    )
                                                    on:click=move |_| {
                                                        if !is_dir {
                                                            on_click(path_click.clone());
                                                        }
                                                    }
                                                >
                                                    <div class=format!(
                                                        "w-8 h-8 rounded-lg flex items-center justify-center shrink-0 {}",
                                                        if is_dir { "bg-yellow-500/20 text-yellow-300" } else { "bg-blue-500/20 text-blue-300" }
                                                    )>
                                                        {if is_dir {
                                                            view! {
                                                                <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M3 7v10a2 2 0 002 2h14a2 2 0 002-2V9a2 2 0 00-2-2h-6l-2-2H5a2 2 0 00-2 2z" />
                                                                </svg>
                                                            }.into_any()
                                                        } else {
                                                            view! {
                                                                <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                                                                </svg>
                                                            }.into_any()
                                                        }}
                                                    </div>
                                                    <div class="flex-1 min-w-0">
                                                        <div class="font-medium text-white/90 truncate text-sm">{file.name}</div>
                                                        <div class="text-xs text-white/40">{size_str}</div>
                                                    </div>
                                                </button>
                                            }
                                        }).collect::<Vec<_>>()}
                                    </div>
                                }.into_any()
                            }
                        }}
                    </div>

                    // File preview
                    <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
                        <div class="px-6 py-4 border-b border-white/10">
                            <h2 class="font-semibold text-white/90">
                                {move || selected_file.get().map(|p| {
                                    p.split('/').next_back().unwrap_or("Preview").to_string()
                                }).unwrap_or_else(|| "File Preview".to_string())}
                            </h2>
                        </div>
                        <div class="p-4 max-h-[600px] overflow-auto">
                            {move || {
                                let content = file_content.get();
                                if content.is_empty() {
                                    view! {
                                        <div class="text-center py-12 text-white/30">
                                            "Select a file to preview its content"
                                        </div>
                                    }.into_any()
                                } else {
                                    view! {
                                        <pre class="text-sm text-white/80 font-mono whitespace-pre-wrap leading-relaxed">{content}</pre>
                                    }.into_any()
                                }
                            }}
                        </div>
                    </div>
                </div>
            </Show>
        </div>
    }
}
