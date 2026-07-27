use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use crate::components::update_banner::{
    abandon_stalled, check_now, request_restart, start_install, UpdaterActivity, UpdaterContext,
};
use crate::services::appearance_service::*;
use crate::services::updater_service::format_bytes;

/// Helper to get checked state from checkbox event
fn event_target_checked(ev: &leptos::ev::Event) -> bool {
    ev.target()
        .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.checked())
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SettingsTab {
    General,
    Storage,
    Jujutsu,
    Theme,
    Updates,
}

impl SettingsTab {
    fn title(&self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Storage => "Storage",
            SettingsTab::Jujutsu => "Jujutsu",
            SettingsTab::Theme => "Theme",
            SettingsTab::Updates => "Updates",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            SettingsTab::General => "M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z M15 12a3 3 0 11-6 0 3 3 0 016 0z",
            // Database / stacked-discs outline.
            SettingsTab::Storage => "M4 7v10c0 2.21 3.582 4 8 4s8-1.79 8-4V7M4 7c0 2.21 3.582 4 8 4s8-1.79 8-4M4 7c0-2.21 3.582-4 8-4s8 1.79 8 4m0 5c0 2.21-3.582 4-8 4s-8-1.79-8-4",
            SettingsTab::Jujutsu => "M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15",
            SettingsTab::Theme => "M20.354 15.354A9 9 0 018.646 3.646 9.003 9.003 0 0012 21a9.003 9.003 0 008.354-5.646z",
            // Download-into-tray. Deliberately not the Jujutsu refresh arrows:
            // two tabs sharing one glyph makes the rail unreadable.
            SettingsTab::Updates => "M12 3v11m0 0l-4-4m4 4l4-4M4 15v3a2 2 0 002 2h12a2 2 0 002-2v-3",
        }
    }
}

#[component]
pub fn Settings(
    /// The project whose storage is being configured. Empty when no project is
    /// selected, which the Storage tab renders as an explanatory empty state
    /// rather than a control that silently does nothing.
    #[prop(default = String::new())]
    project_id: String,
) -> impl IntoView {
    let (active_tab, set_active_tab) = signal(SettingsTab::General);

    let (theme_id, set_theme_id) = signal("default".to_string());
    let (appearance_mode, set_appearance_mode_signal) = signal(AppearanceMode::Dark);
    let (themes, set_themes) = signal(Vec::<Theme>::new());

    // Load themes and current theme on mount
    let load_themes = {
        move || {
            spawn_local(async move {
                if let Ok(loaded_themes) = list_themes().await {
                    set_themes.set(loaded_themes);
                }
                if let Ok(current_theme) = get_theme().await {
                    set_theme_id.set(current_theme.id.clone());
                    apply_theme_to_dom(&current_theme);
                }
                if let Ok(mode) = get_appearance_mode().await {
                    set_appearance_mode_signal.set(mode);
                }
            });
        }
    };

    // Load themes once on mount
    let _ = Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true; // Only run once
        }
        load_themes();
        true
    });

    let (git_colocation, set_git_colocation) = signal(true);

    let (diff_collapsed, set_diff_collapsed) = signal({
        web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item("slashit_diff_default_collapsed").ok().flatten())
            .map(|v| v == "true")
            .unwrap_or(false)
    });

    view! {
        <div class="space-y-6">
            <div class="mb-6">
                <h1 class="text-2xl font-bold text-white/90">"Settings"</h1>
                <p class="text-sm text-white/40 mt-1">"Configure your application preferences"</p>
            </div>

            <div class="flex gap-6">
                <div class="w-64 shrink-0">
                    <div class="border border-white/10 rounded-xl bg-white/[0.02] overflow-hidden">
                        <div class="p-2 space-y-1">
                            {[
                                SettingsTab::General,
                                SettingsTab::Storage,
                                SettingsTab::Jujutsu,
                                SettingsTab::Theme,
                                SettingsTab::Updates,
                            ].into_iter().map(|tab| {
                                let is_active = active_tab.get() == tab;
                                let tab_clone = tab;
                                view! {
                                    <button
                                        on:click=move |_| set_active_tab.set(tab_clone)
                                        class=format!(
                                            "w-full flex items-center gap-3 px-4 py-3 rounded-lg text-left transition-all {}",
                                            if is_active {
                                                "bg-blue-500/20 text-blue-300"
                                            } else {
                                                "text-white/60 hover:text-white/90 hover:bg-white/5"
                                            }
                                        )
                                    >
                                        <svg class="w-5 h-5 shrink-0" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="1.5">
                                            <path stroke-linecap="round" stroke-linejoin="round" d={tab_clone.icon()} />
                                        </svg>
                                        <span class="font-medium">{tab_clone.title()}</span>
                                    </button>
                                }
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>
                </div>

                <div class="flex-1">
                    <div class="border border-white/10 rounded-xl bg-white/[0.02] p-6">
                        {move || {
                            match active_tab.get() {
                                SettingsTab::General => view! {
                                    <div class="space-y-6">
                                        <div>
                                            <h2 class="text-lg font-semibold text-white/90 mb-4">"General Settings"</h2>
                                            <p class="text-sm text-white/50 mb-4">"Theme and appearance settings are available in the Theme tab."</p>

                                            <div class="flex items-center justify-between p-4 rounded-xl bg-white/5 border border-white/10">
                                                <div>
                                                    <p class="font-medium text-white/90">"Collapse diff files by default"</p>
                                                    <p class="text-sm text-white/40 mt-1">"When opening a diff, show file headers only. You can still toggle each file or use Expand all."</p>
                                                </div>
                                                <label class="relative inline-flex items-center cursor-pointer">
                                                    <input
                                                        type="checkbox"
                                                        prop:checked=diff_collapsed
                                                        on:change=move |ev| {
                                                            let v = event_target_checked(&ev);
                                                            set_diff_collapsed.set(v);
                                                            if let Some(window) = web_sys::window() {
                                                                if let Ok(Some(storage)) = window.local_storage() {
                                                                    let _ = storage.set_item(
                                                                        "slashit_diff_default_collapsed",
                                                                        if v { "true" } else { "false" },
                                                                    );
                                                                }
                                                            }
                                                        }
                                                        class="sr-only peer"
                                                    />
                                                    <div class="w-11 h-6 bg-white/10 peer-focus:outline-none peer-focus:ring-2 peer-focus:ring-blue-500/50 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-blue-500"></div>
                                                </label>
                                            </div>
                                        </div>
                                    </div>
                                }.into_any(),

                                SettingsTab::Storage => view! {
                                    <crate::components::StorageSettings project_id=project_id.clone() />
                                }.into_any(),

                                SettingsTab::Jujutsu => view! {
                                    <div class="space-y-6">
                                        <div>
                                            <h2 class="text-lg font-semibold text-white/90 mb-4">"Jujutsu Configuration"</h2>
                                            <div class="space-y-4">
                                                <div class="p-4 rounded-xl bg-blue-500/10 border border-blue-500/20">
                                                    <p class="text-sm text-blue-300 font-medium mb-2">"User configuration"</p>
                                                    <p class="text-sm text-white/60">
                                                        "Jujutsu user configuration is managed via the jj CLI. To set your identity, run in a terminal:"
                                                    </p>
                                                    <div class="mt-3 space-y-1">
                                                        <code class="block text-xs text-white/80 bg-white/5 px-3 py-2 rounded-lg font-mono">
                                                            "jj config set --user user.name 'Your Name'"
                                                        </code>
                                                        <code class="block text-xs text-white/80 bg-white/5 px-3 py-2 rounded-lg font-mono">
                                                            "jj config set --user user.email 'you@example.com'"
                                                        </code>
                                                    </div>
                                                </div>
                                                <div class="flex items-center justify-between p-4 rounded-xl bg-white/5 border border-white/10">
                                                    <div>
                                                        <p class="font-medium text-white/90">"Enable Git Colocation"</p>
                                                        <p class="text-sm text-white/40 mt-1">"Use Git as backend for Jujutsu"</p>
                                                    </div>
                                                    <label class="relative inline-flex items-center cursor-pointer">
                                                        <input
                                                            type="checkbox"
                                                            prop:checked=git_colocation
                                                            on:change=move |ev| set_git_colocation.set(event_target_checked(&ev))
                                                            class="sr-only peer"
                                                        />
                                                        <div class="w-11 h-6 bg-white/10 peer-focus:outline-none peer-focus:ring-2 peer-focus:ring-blue-500/50 rounded-full peer peer-checked:after:translate-x-full peer-checked:after:border-white after:content-[''] after:absolute after:top-[2px] after:left-[2px] after:bg-white after:rounded-full after:h-5 after:w-5 after:transition-all peer-checked:bg-blue-500"></div>
                                                    </label>
                                                </div>
                                            </div>
                                        </div>
                                    </div>
                                }.into_any(),

                                SettingsTab::Theme => view! {
                                    <div class="space-y-6">
                                        <div>
                                            <h2 class="text-lg font-semibold text-white/90 mb-4">"Appearance Mode"</h2>
                                            <div class="space-y-4">
                                                <div>
                                                    <label class="block text-sm font-medium text-white/70 mb-3">"Mode"</label>
                                                    <div class="flex gap-4">
                                                        {[
                                                            (AppearanceMode::System, "System", "Follow system preference"),
                                                            (AppearanceMode::Light, "Light", "Always use light mode"),
                                                            (AppearanceMode::Dark, "Dark", "Always use dark mode"),
                                                        ].into_iter().map(|(mode, label, description)| {
                                                            let is_selected = appearance_mode.get() == mode;
                                                            let mode_value = mode.clone();
                                                            view! {
                                                                <button
                                                                    on:click=move |_| {
                                                                        let mode_clone = mode_value.clone();
                                                                        spawn_local(async move {
                                                                            if let Err(e) = set_appearance_mode(mode_clone.clone()).await {
                                                                                eprintln!("Failed to set appearance mode: {}", e);
                                                                            } else {
                                                                                set_appearance_mode_signal.set(mode_clone);
                                                                            }
                                                                        });
                                                                    }
                                                                    class=format!(
                                                                        "relative p-4 rounded-xl border-2 transition-all text-left {}",
                                                                        if is_selected {
                                                                            "border-yellow-500 bg-yellow-500/10"
                                                                        } else {
                                                                            "border-white/10 hover:border-white/20 bg-white/5"
                                                                        }
                                                                    )
                                                                >
                                                                    <div class="text-left">
                                                                        <p class="font-medium text-white/90">{label}</p>
                                                                        <p class="text-xs text-white/50 mt-1">{description}</p>
                                                                    </div>
                                                                    {is_selected.then(|| view! {
                                                                        <div class="absolute top-2 right-2 w-4 h-4 rounded-full bg-yellow-500 flex items-center justify-center">
                                                                            <svg class="w-3 h-3 text-black" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="3" d="M5 13l4 4L19 7" />
                                                                            </svg>
                                                                        </div>
                                                                    })}
                                                                </button>
                                                            }
                                                        }).collect::<Vec<_>>()}
                                                    </div>
                                                </div>

                                                <div>
                                                    <label class="block text-sm font-medium text-white/70 mb-3">"Color Theme"</label>
                                                    <div class="grid grid-cols-2 gap-4">
                                                        {move || {
                                                            themes.get().into_iter().map(|theme| {
                                                                let is_selected = theme_id.get() == theme.id;
                                                                let theme_clone = theme.clone();
                                                                let id = theme.id.clone();
                                                                view! {
                                                                    <button
                                                                        on:click=move |_| {
                                                                            let id_clone = id.clone();
                                                                            spawn_local(async move {
                                                                                if let Err(e) = set_theme_by_id(id_clone.clone()).await {
                                                                                    eprintln!("Failed to set theme: {}", e);
                                                                                } else {
                                                                                    set_theme_id.set(id_clone.clone());
                                                                                    if let Ok(theme) = get_theme().await {
                                                                                        apply_theme_to_dom(&theme);
                                                                                    }
                                                                                }
                                                                            });
                                                                        }
                                                                        class=format!(
                                                                            "relative p-4 rounded-xl border-2 transition-all text-left {}",
                                                                            if is_selected {
                                                                                "border-yellow-500 bg-yellow-500/10"
                                                                            } else {
                                                                                "border-white/10 hover:border-white/20 bg-white/5"
                                                                            }
                                                                        )
                                                                    >
                                                                        <div class="flex items-center gap-2 mb-2">
                                                                            <div class="w-4 h-4 rounded-full" style=format!("background-color: {}", theme_clone.colors.accent)></div>
                                                                            <p class="font-medium text-white/90">{theme_clone.name.clone()}</p>
                                                                        </div>
                                                                        <p class="text-xs text-white/50">{theme_clone.description.clone()}</p>
                                                                        {is_selected.then(|| view! {
                                                                            <div class="absolute top-2 right-2 w-4 h-4 rounded-full bg-yellow-500 flex items-center justify-center">
                                                                                <svg class="w-3 h-3 text-black" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="3" d="M5 13l4 4L19 7" />
                                                                                </svg>
                                                                            </div>
                                                                        })}
                                                                    </button>
                                                                }
                                                            }).collect::<Vec<_>>()
                                                        }}
                                                    </div>
                                                </div>

                                            </div>
                                        </div>
                                    </div>
                                }.into_any(),

                                SettingsTab::Updates => view! {
                                    <UpdatesTab />
                                }.into_any(),

                            }
                        }}
                    </div>
                </div>
            </div>
        </div>
    }
}

/// Settings > Updates.
///
/// Reads the same `UpdaterContext` the floating notice does, so the two can
/// never contradict each other and a message here cannot survive the check that
/// disproved it: `run_check` clears the failure before every attempt.
#[component]
fn UpdatesTab() -> impl IntoView {
    let Some(ctx) = use_context::<UpdaterContext>() else {
        return view! {
            <div class="space-y-6">
                <h2 class="text-lg font-semibold text-white/90">"Application updates"</h2>
                <p class="text-sm text-red-300">
                    "Update state is unavailable in this window. Restart SlashIt; if it persists, this is a bug."
                </p>
            </div>
        }
        .into_any();
    };

    view! {
        <div class="space-y-6">
            <div>
                <h2 class="text-lg font-semibold text-white/90 mb-4">"Application updates"</h2>
                <div class="space-y-4">
                    <div class="p-4 rounded-xl bg-white/5 border border-white/10">
                        <div class="flex items-start justify-between gap-4">
                            <div class="min-w-0">
                                <p class="text-xs uppercase tracking-wide text-white/40">"Current version"</p>
                                <p class="font-medium text-white/90 mt-1">
                                    // Never guess a version. "Unknown" is a real,
                                    // reportable answer; a fake number is not.
                                    {move || ctx.current_version().unwrap_or_else(|| "Unknown".to_string())}
                                </p>
                                {move || ctx.last_check.get().map(|ts| view! {
                                    <p class="text-xs text-white/40 mt-2">{format!("Last checked: {ts}")}</p>
                                })}
                            </div>
                            {move || {
                                // No check button on a build that cannot act on
                                // the answer.
                                if ctx.blocked_reason().is_some() {
                                    return ().into_any();
                                }
                                let busy = ctx.activity.get().busy();
                                let checking = ctx.activity.get() == UpdaterActivity::Checking;
                                view! {
                                    <button
                                        on:click=move |_| check_now(ctx)
                                        disabled=busy
                                        class="shrink-0 px-3 py-2 rounded-lg bg-white/10 hover:bg-white/15 text-sm text-white/80 disabled:opacity-50 disabled:cursor-not-allowed"
                                    >
                                        {if checking { "Checking..." } else { "Check for updates" }}
                                    </button>
                                }.into_any()
                            }}
                        </div>
                    </div>

                    // Unsupported or disabled: the reason replaces every action.
                    {move || ctx.blocked_reason().map(|reason| view! {
                        <div class="p-4 rounded-xl bg-white/5 border border-white/15">
                            <p class="font-medium text-white/80">"In-app updates are unavailable"</p>
                            <p class="text-sm text-white/50 mt-1">{reason}</p>
                        </div>
                    })}

                    {move || {
                        let Some(failure) = ctx.failure.get() else {
                            return ().into_any();
                        };
                        let headline = failure.kind.headline();
                        let hint = failure.kind.hint();
                        let detail = format!("{}: {}", failure.stage.label(), failure.detail);
                        view! {
                            <div class="p-4 rounded-xl bg-red-500/10 border border-red-500/30">
                                <p class="font-medium text-red-200">{headline}</p>
                                <p class="text-sm text-white/60 mt-1">{hint}</p>
                                <p class="text-xs text-white/35 mt-2 font-mono break-words">{detail}</p>
                            </div>
                        }.into_any()
                    }}

                    // A completed check that found nothing is a result worth
                    // stating; silence would read as "never checked".
                    {move || ctx.up_to_date().then(|| view! {
                        <div class="p-4 rounded-xl bg-white/5 border border-white/10">
                            <p class="text-sm text-white/70">"SlashIt is up to date."</p>
                        </div>
                    })}

                    {move || {
                        match ctx.activity.get() {
                            UpdaterActivity::Downloading { downloaded, total } => {
                                let percent = total
                                    .filter(|t| *t > 0)
                                    .map(|t| ((downloaded as f64 / t as f64) * 100.0).clamp(0.0, 100.0));
                                let detail = match (percent, total) {
                                    (Some(p), Some(t)) => format!(
                                        "{} of {} ({:.0}%)",
                                        format_bytes(downloaded),
                                        format_bytes(t),
                                        p
                                    ),
                                    _ => format!(
                                        "{} downloaded (total size not reported)",
                                        format_bytes(downloaded)
                                    ),
                                };
                                view! {
                                    <div class="p-4 rounded-xl bg-blue-500/10 border border-blue-500/30">
                                        <p class="font-medium text-blue-200">"Downloading update"</p>
                                        <p class="text-xs text-white/50 mt-1">{detail}</p>
                                        <div class="mt-3 h-1.5 rounded-full bg-white/10 overflow-hidden">
                                            {match percent {
                                                Some(p) => view! {
                                                    <div class="h-full rounded-full bg-blue-400 transition-all" style=format!("width: {p:.1}%")></div>
                                                }.into_any(),
                                                None => view! {
                                                    <div class="h-full w-1/3 rounded-full bg-blue-400 animate-pulse"></div>
                                                }.into_any(),
                                            }}
                                        </div>
                                        {ctx.stalled.get().then(|| view! {
                                            <div class="mt-3 flex items-center justify-between gap-3">
                                                <p class="text-xs text-amber-300">"No progress for a while. It may still be running."</p>
                                                <button
                                                    on:click=move |_| abandon_stalled(ctx)
                                                    class="shrink-0 px-2.5 py-1 rounded-lg text-xs text-white/70 hover:text-white/90 hover:bg-white/5"
                                                >
                                                    "Hide"
                                                </button>
                                            </div>
                                        })}
                                    </div>
                                }.into_any()
                            }
                            UpdaterActivity::Installing => view! {
                                <div class="p-4 rounded-xl bg-blue-500/10 border border-blue-500/30">
                                    <p class="font-medium text-blue-200">"Installing update"</p>
                                    <p class="text-xs text-white/50 mt-1">"SlashIt keeps running until you restart it."</p>
                                    {ctx.stalled.get().then(|| view! {
                                        <div class="mt-3 flex items-center justify-between gap-3">
                                            <p class="text-xs text-amber-300">"This is taking longer than expected."</p>
                                            <button
                                                on:click=move |_| abandon_stalled(ctx)
                                                class="shrink-0 px-2.5 py-1 rounded-lg text-xs text-white/70 hover:text-white/90 hover:bg-white/5"
                                            >
                                                "Hide"
                                            </button>
                                        </div>
                                    })}
                                </div>
                            }.into_any(),
                            UpdaterActivity::RestartRequired => view! {
                                <div class="p-4 rounded-xl bg-emerald-500/10 border border-emerald-500/30">
                                    <div class="flex items-center justify-between gap-4">
                                        <div class="min-w-0">
                                            <p class="font-medium text-emerald-200">"Restart to finish"</p>
                                            <p class="text-xs text-white/50 mt-1">
                                                "The update is installed but not running yet. Restarting closes every terminal and agent."
                                            </p>
                                        </div>
                                        <button
                                            on:click=move |_| request_restart(ctx)
                                            class="shrink-0 px-3 py-2 rounded-lg bg-emerald-500 text-black text-sm font-semibold hover:bg-emerald-400"
                                        >
                                            "Restart now"
                                        </button>
                                    </div>
                                </div>
                            }.into_any(),
                            UpdaterActivity::Restarting => view! {
                                <div class="p-4 rounded-xl bg-emerald-500/10 border border-emerald-500/30">
                                    <p class="font-medium text-emerald-200">"Restarting SlashIt..."</p>
                                </div>
                            }.into_any(),
                            UpdaterActivity::Idle | UpdaterActivity::Checking => ().into_any(),
                        }
                    }}

                    {move || {
                        // Hidden while an install is in flight so the page can
                        // never offer to start a second one.
                        if ctx.activity.get().busy() {
                            return ().into_any();
                        }
                        let Some(update) = ctx.available.get() else {
                            return ().into_any();
                        };
                        let blocked = ctx.blocked_reason();
                        let body = update.body.clone().unwrap_or_default();
                        view! {
                            <div class="p-4 rounded-xl bg-amber-500/10 border border-amber-500/30">
                                <div class="flex items-start justify-between gap-4 mb-3">
                                    <div class="min-w-0">
                                        <p class="font-semibold text-amber-200">
                                            {format!("SlashIt {} is available", update.version)}
                                        </p>
                                        <p class="text-xs text-white/50 mt-1">
                                            {format!("You are running {}.", update.current_version)}
                                        </p>
                                        {update.date.clone().map(|d| view! {
                                            <p class="text-xs text-white/35 mt-1">{format!("Released {d}")}</p>
                                        })}
                                    </div>
                                    {match blocked {
                                        Some(reason) => view! {
                                            <p class="shrink-0 max-w-[16rem] text-xs text-amber-200/80 text-right">{reason}</p>
                                        }.into_any(),
                                        None => view! {
                                            <button
                                                on:click=move |_| start_install(ctx)
                                                class="shrink-0 px-3 py-2 rounded-lg bg-amber-500 text-black text-sm font-semibold hover:bg-amber-400"
                                            >
                                                "Download and install"
                                            </button>
                                        }.into_any(),
                                    }}
                                </div>
                                {(!body.is_empty()).then(|| view! {
                                    <pre class="text-xs text-white/70 bg-black/30 rounded-lg p-3 max-h-64 overflow-auto whitespace-pre-wrap">
                                        {body.clone()}
                                    </pre>
                                })}
                            </div>
                        }.into_any()
                    }}
                </div>
            </div>
        </div>
    }
    .into_any()
}
