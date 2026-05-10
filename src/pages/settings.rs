use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use crate::services::appearance_service::*;

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
    Jujutsu,
    Theme,
}

impl SettingsTab {
    fn title(&self) -> &'static str {
        match self {
            SettingsTab::General => "General",
            SettingsTab::Jujutsu => "Jujutsu",
            SettingsTab::Theme => "Theme",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            SettingsTab::General => "M10.325 4.317c.426-1.756 2.924-1.756 3.35 0a1.724 1.724 0 002.573 1.066c1.543-.94 3.31.826 2.37 2.37a1.724 1.724 0 001.065 2.572c1.756.426 1.756 2.924 0 3.35a1.724 1.724 0 00-1.066 2.573c.94 1.543-.826 3.31-2.37 2.37a1.724 1.724 0 00-2.572 1.065c-.426 1.756-2.924 1.756-3.35 0a1.724 1.724 0 00-2.573-1.066c-1.543.94-3.31-.826-2.37-2.37a1.724 1.724 0 00-1.065-2.572c-1.756-.426-1.756-2.924 0-3.35a1.724 1.724 0 001.066-2.573c-.94-1.543.826-3.31 2.37-2.37.996.608 2.296.07 2.572-1.065z M15 12a3 3 0 11-6 0 3 3 0 016 0z",
            SettingsTab::Jujutsu => "M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15",
            SettingsTab::Theme => "M20.354 15.354A9 9 0 018.646 3.646 9.003 9.003 0 0012 21a9.003 9.003 0 008.354-5.646z",
        }
    }
}

#[component]
pub fn Settings() -> impl IntoView {
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
                                SettingsTab::Jujutsu,
                                SettingsTab::Theme,
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
                                            <p class="text-sm text-white/50">"Theme and appearance settings are available in the Theme tab."</p>
                                        </div>
                                    </div>
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

                            }
                        }}
                    </div>
                </div>
            </div>
        </div>
    }
}
