// SPDX-License-Identifier: MPL-2.0
//! The About SlashIt dialog, opened from Settings.
//!
//! Shows what the running application reports about itself, and copies exactly
//! those facts as plain text for a bug report. Every value comes from the
//! backend (`app_about`); nothing about the version or commit is written here.

use crate::services::updater_service::{app_about, AppAbout};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["navigator", "clipboard"], js_name = writeText, catch)]
    fn clipboard_write_text(text: &str) -> Result<js_sys::Promise, JsValue>;
}

/// What the dialog shows. Opening starts from `Loading`; closing drops whatever
/// was loaded, so a reopened dialog never shows facts from an earlier read.
#[derive(Debug, Clone, PartialEq)]
pub enum AboutView {
    Hidden,
    Loading,
    Loaded(AppAbout),
    Failed(String),
}

impl AboutView {
    pub fn for_visibility(showing: bool) -> Self {
        if showing {
            Self::Loading
        } else {
            Self::Hidden
        }
    }
}

/// The labelled facts the dialog shows, in display order. The Copy Diagnostics
/// text is built from this same list, so the two cannot disagree.
pub fn about_fields(about: &AppAbout) -> Vec<(&'static str, String)> {
    vec![
        ("Product", about.product_name.clone()),
        ("Version", about.version.clone()),
        ("Build type", about.build_type.clone()),
        ("Source commit", about.source_commit.clone().unwrap_or_else(|| "Unknown".to_string())),
        ("Operating system", about.operating_system.clone()),
        ("Executable", about.executable_path.clone().unwrap_or_else(|| "Unknown".to_string())),
        ("Data directory", about.data_dir.clone()),
    ]
}

/// The plain-text block Copy Diagnostics puts on the clipboard: one
/// `Label: value` line per field, and nothing else. A value cannot break its
/// line: a newline in a path (legal on Linux) is written as a space.
pub fn diagnostics_text(about: &AppAbout) -> String {
    about_fields(about)
        .into_iter()
        .map(|(label, value)| {
            let value = value.replace(['\n', '\r'], " ");
            format!("{label}: {value}\n")
        })
        .collect()
}

#[component]
pub fn AboutSlashItDialog(show: RwSignal<bool>) -> impl IntoView {
    let view_state = RwSignal::new(AboutView::Hidden);
    let copy_status = RwSignal::new(None::<String>);
    // Every open or close starts a new generation. A read started by an earlier
    // open is stale once the generation has moved on, and may not write its answer.
    let generation = Rc::new(Cell::new(0u64));

    Effect::new(move |_| {
        let showing = show.get();
        let this_generation = generation.get().wrapping_add(1);
        generation.set(this_generation);
        view_state.set(AboutView::for_visibility(showing));
        copy_status.set(None);
        if showing {
            let generation = Rc::clone(&generation);
            spawn_local(async move {
                let result = app_about().await;
                // Only the read of the dialog still open, from this open, may take the answer.
                if generation.get() == this_generation && show.get_untracked() {
                    view_state.set(match result {
                        Ok(about) => AboutView::Loaded(about),
                        Err(error) => AboutView::Failed(error),
                    });
                }
            });
        }
    });

    let copy_diagnostics = move |_| {
        let AboutView::Loaded(about) = view_state.get_untracked() else {
            return;
        };
        let text = diagnostics_text(&about);
        spawn_local(async move {
            let outcome = match clipboard_write_text(&text) {
                Ok(promise) => JsFuture::from(promise)
                    .await
                    .map(|_| ())
                    .map_err(|_| "the clipboard refused the text".to_string()),
                Err(_) => Err("the clipboard is not available".to_string()),
            };
            copy_status.set(Some(match outcome {
                Ok(()) => "Copied to the clipboard.".to_string(),
                Err(error) => format!("Could not copy: {error}"),
            }));
        });
    };

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| show.set(false)
                ></div>

                <div
                    data-testid="about-slashit-dialog"
                    role="dialog"
                    aria-modal="true"
                    aria-labelledby="about-slashit-title"
                    class="relative w-full max-w-md bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl"
                    on:click=move |e| e.stop_propagation()
                >
                    <div class="flex items-center justify-between p-6 border-b border-white/5">
                        <h2 id="about-slashit-title" class="text-lg font-semibold text-white/90">"About SlashIt"</h2>
                        <button
                            on:click=move |_| show.set(false)
                            class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                            aria-label="Close"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="p-6 space-y-4">
                        {move || match view_state.get() {
                            AboutView::Hidden | AboutView::Loading => view! {
                                <p class="text-sm text-white/40">"Reading application details..."</p>
                            }.into_any(),
                            AboutView::Failed(error) => view! {
                                <div role="alert" data-testid="about-slashit-error" class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm">
                                    {format!("Could not read application details: {error}")}
                                </div>
                            }.into_any(),
                            AboutView::Loaded(about) => view! {
                                <dl data-testid="about-slashit-fields" class="space-y-2">
                                    {about_fields(&about).into_iter().map(|(label, value)| view! {
                                        <div class="flex gap-3 text-sm">
                                            <dt class="w-36 shrink-0 text-white/50">{label}</dt>
                                            <dd class="min-w-0 break-all font-mono text-xs text-white/80 pt-0.5">{value}</dd>
                                        </div>
                                    }).collect_view()}
                                </dl>
                            }.into_any(),
                        }}

                        {move || copy_status.get().map(|message| view! {
                            <p role="status" data-testid="about-slashit-copy-status" class="text-xs text-white/50">{message}</p>
                        })}
                    </div>

                    <div class="flex items-center justify-end gap-3 p-6 border-t border-white/5">
                        <button
                            data-testid="about-slashit-copy"
                            on:click=copy_diagnostics
                            disabled=move || !matches!(view_state.get(), AboutView::Loaded(_))
                            class="px-4 py-2 rounded-lg bg-white/5 hover:bg-white/10 text-white/80 font-medium transition-colors disabled:text-white/30 disabled:cursor-not-allowed"
                        >
                            "Copy Diagnostics"
                        </button>
                        <button
                            on:click=move |_| show.set(false)
                            class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                        >
                            "Close"
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn about(commit: Option<&str>, executable: Option<&str>) -> AppAbout {
        AppAbout {
            product_name: "SlashIt".to_string(),
            version: "0.0.1".to_string(),
            build_type: "release".to_string(),
            source_commit: commit.map(str::to_string),
            operating_system: "linux".to_string(),
            executable_path: executable.map(str::to_string),
            data_dir: "/home/user/.local/share/slashit".to_string(),
        }
    }

    #[test]
    fn visibility_selects_the_loading_or_hidden_state() {
        // Only the state mapping is tested here. That closing clears the loaded
        // facts and the copy status is done by the Effect, which needs a
        // rendering harness to test.
        assert_eq!(AboutView::for_visibility(true), AboutView::Loading);
        assert_eq!(AboutView::for_visibility(false), AboutView::Hidden);
    }

    #[test]
    fn a_newline_in_a_value_cannot_split_a_diagnostics_line() {
        let text = diagnostics_text(&about(None, Some("/tmp/odd\nname\r\nbin")));
        assert!(text.contains("Executable: /tmp/odd name  bin\n"), "{text}");
        assert_eq!(text.lines().count(), 7, "{text}");
    }

    #[test]
    fn the_frontend_reads_the_backend_serialisation_of_the_about_facts() {
        // The same JSON the backend's `the_about_facts_serialise_with_the_names_the_frontend_reads` test pins.
        let json = r#"{"productName":"SlashIt","version":"0.0.1","buildType":"development","sourceCommit":null,"operatingSystem":"linux","executablePath":"/opt/slashit/slashit-ui","dataDir":"/home/user/.local/share/slashit"}"#;
        let about: AppAbout = serde_json::from_str(json).unwrap();
        assert_eq!(about.product_name, "SlashIt");
        assert_eq!(about.source_commit, None);
        assert_eq!(about.executable_path.as_deref(), Some("/opt/slashit/slashit-ui"));
        assert_eq!(about.data_dir, "/home/user/.local/share/slashit");
    }

    #[test]
    fn the_diagnostics_block_holds_exactly_the_about_fields_in_order() {
        let text = diagnostics_text(&about(Some("abc123"), Some("/opt/slashit/slashit-ui")));
        let labels: Vec<&str> = text
            .lines()
            .map(|line| line.split(": ").next().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "Product",
                "Version",
                "Build type",
                "Source commit",
                "Operating system",
                "Executable",
                "Data directory",
            ]
        );
        assert!(text.contains("Source commit: abc123\n"));
        assert!(text.contains("Executable: /opt/slashit/slashit-ui\n"));
    }

    #[test]
    fn a_missing_commit_or_executable_is_reported_as_unknown_not_invented() {
        let text = diagnostics_text(&about(None, None));
        assert!(text.contains("Source commit: Unknown\n"), "{text}");
        assert!(text.contains("Executable: Unknown\n"), "{text}");
    }
}
