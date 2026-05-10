use leptos::prelude::*;

#[derive(Clone, Debug)]
struct DiffFile {
    path: String,
    additions: usize,
    deletions: usize,
    lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
struct DiffLine {
    kind: LineKind,
    content: String,
}

#[derive(Clone, Debug, PartialEq)]
enum LineKind {
    Addition,
    Deletion,
    Context,
    Header,
}

fn parse_diff(raw: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;

    for line in raw.lines() {
        if line.starts_with("diff --git") {
            if let Some(f) = current.take() {
                files.push(f);
            }
            // Extract filename from "diff --git a/path b/path"
            let path = line
                .split(" b/")
                .last()
                .unwrap_or("unknown")
                .to_string();
            current = Some(DiffFile {
                path,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
        } else if let Some(ref mut f) = current {
            if line.starts_with("@@") || line.starts_with("+++") || line.starts_with("---") {
                f.lines.push(DiffLine {
                    kind: LineKind::Header,
                    content: line.to_string(),
                });
            } else if line.starts_with('+') {
                f.additions += 1;
                f.lines.push(DiffLine {
                    kind: LineKind::Addition,
                    content: line.to_string(),
                });
            } else if line.starts_with('-') {
                f.deletions += 1;
                f.lines.push(DiffLine {
                    kind: LineKind::Deletion,
                    content: line.to_string(),
                });
            } else {
                f.lines.push(DiffLine {
                    kind: LineKind::Context,
                    content: line.to_string(),
                });
            }
        }
    }

    if let Some(f) = current {
        files.push(f);
    }

    files
}

#[component]
pub fn DiffViewer(
    diff: String,
    #[prop(optional)] stat: Option<String>,
) -> impl IntoView {
    let files = parse_diff(&diff);
    let total_additions: usize = files.iter().map(|f| f.additions).sum();
    let total_deletions: usize = files.iter().map(|f| f.deletions).sum();
    let file_count = files.len();

    if file_count == 0 {
        return view! {
            <div class="diff-viewer">
                <div class="flex flex-col items-center justify-center py-12 text-center">
                    <svg class="w-12 h-12 text-white/10 mb-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="1.5" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z" />
                    </svg>
                    <p class="text-white/40 text-sm">"No changes detected"</p>
                    <p class="text-white/20 text-xs mt-1">"The working directory matches the last commit"</p>
                </div>
            </div>
        }.into_any();
    }

    view! {
        <div class="diff-viewer space-y-3">
            // Summary header
            <div class="flex items-center gap-3 px-3 py-2 rounded-lg bg-white/[0.03] border border-white/[0.06]">
                <span class="text-sm text-white/60">
                    {file_count} " file" {if file_count != 1 { "s" } else { "" }} " changed"
                </span>
                <span class="text-sm text-emerald-400">{"+"}{total_additions}</span>
                <span class="text-sm text-red-400">{"-"}{total_deletions}</span>
                {stat.map(|s| view! {
                    <span class="text-xs text-white/40 ml-auto font-mono">{s}</span>
                })}
            </div>

            // File sections
            {files.into_iter().map(|file| {
                let path = file.path.clone();
                let adds = file.additions;
                let dels = file.deletions;
                let (collapsed, set_collapsed) = signal(false);

                view! {
                    <div class="rounded-lg border border-white/[0.06] overflow-hidden">
                        // File header
                        <button
                            class="w-full flex items-center gap-2 px-3 py-2 bg-white/[0.03] hover:bg-white/[0.05] transition-colors text-left"
                            on:click=move |_| set_collapsed.update(|v| *v = !*v)
                        >
                            <span class="text-xs text-white/40">{move || if collapsed.get() { "▶" } else { "▼" }}</span>
                            <span class="text-sm font-mono text-white/80 truncate">{path.clone()}</span>
                            <span class="ml-auto flex items-center gap-2 text-xs">
                                <span class="text-emerald-400">{"+"}{adds}</span>
                                <span class="text-red-400">{"-"}{dels}</span>
                            </span>
                        </button>

                        // Diff lines
                        <Show when=move || !collapsed.get()>
                            <div class="overflow-x-auto">
                                <pre class="text-xs font-mono leading-5 p-0 m-0">
                                    {file.lines.iter().map(|line| {
                                        let (bg, text_color) = match line.kind {
                                            LineKind::Addition => ("bg-emerald-500/10", "text-emerald-300"),
                                            LineKind::Deletion => ("bg-red-500/10", "text-red-300"),
                                            LineKind::Header => ("bg-blue-500/10", "text-blue-300"),
                                            LineKind::Context => ("", "text-white/50"),
                                        };
                                        let content = line.content.clone();
                                        view! {
                                            <div class=format!("px-3 min-h-[1.25rem] {} {}", bg, text_color)>
                                                {content}
                                            </div>
                                        }
                                    }).collect_view()}
                                </pre>
                            </div>
                        </Show>
                    </div>
                }
            }).collect_view()}
        </div>
    }.into_any()
}

/// Modal wrapper for viewing diffs
#[component]
pub fn DiffModal(
    show: RwSignal<bool>,
    diff: Signal<String>,
    stat: Signal<String>,
    title: String,
) -> impl IntoView {
    view! {
        <Show when=move || show.get()>
            {
                let title = title.clone();
                let diff_val = diff.get();
                let stat_val = stat.get();
                view! {
                    <div
                        class="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
                        on:click=move |_| show.set(false)
                    >
                        <div
                            class="bg-[#0e0e16] border border-white/[0.08] rounded-2xl w-[90vw] max-w-4xl max-h-[85vh] flex flex-col shadow-2xl"
                            on:click=move |e| e.stop_propagation()
                        >
                            <div class="flex items-center justify-between px-5 py-3 border-b border-white/[0.06]">
                                <h2 class="text-sm font-medium text-white/80">{title.clone()}</h2>
                                <button
                                    class="text-white/40 hover:text-white/80 transition-colors text-lg"
                                    on:click=move |_| show.set(false)
                                >
                                    "×"
                                </button>
                            </div>
                            <div class="overflow-y-auto p-4">
                                <DiffViewer
                                    diff=diff_val
                                    stat=stat_val
                                />
                            </div>
                        </div>
                    </div>
                }
            }
        </Show>
    }
}
