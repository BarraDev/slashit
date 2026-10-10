//! Queue settings in the Settings page: what the Queue is running under, and
//! the way to change it.
//!
//! The values shown are the backend's, read when the tab opens. Until they
//! have been read nothing is shown as a setting, and nothing can be edited:
//! offering the defaults as if they were the current values is how someone
//! saves a change on top of settings they never saw.

use crate::components::queue_settings_modal::{describe_queue_config, QueueSettingsModal};
use crate::models::QueueConfig;
use crate::services::get_queue_config;
use leptos::prelude::*;
use leptos::task::spawn_local;

#[component]
pub fn QueueSettings() -> impl IntoView {
    // Only meaningful once `loaded`; the defaults it starts with are never
    // shown as the current settings.
    let config = RwSignal::new(QueueConfig::default());
    let loaded = RwSignal::new(false);
    let load_error = RwSignal::new(None::<String>);
    let show_form = RwSignal::new(false);

    let load = move || {
        load_error.set(None);
        spawn_local(async move {
            match get_queue_config().await {
                Ok(current) => {
                    config.set(current);
                    loaded.set(true);
                }
                Err(e) => load_error.set(Some(e)),
            }
        });
    };

    let _ = Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true;
        }
        load();
        true
    });

    view! {
        <div class="space-y-6" data-testid="queue-settings">
            <div>
                <h2 class="text-lg font-semibold text-white/90 mb-1">"Queue"</h2>
                <p class="text-sm text-white/50">
                    "How many Tasks run at once, and whether queued Tasks start by themselves. "
                    "These settings are saved and apply again after SlashIt restarts."
                </p>
            </div>

            {move || match (loaded.get(), load_error.get()) {
                (true, _) => view! {
                    {move || { let current = config.get(); view! {
                    <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-3">
                        <p class="text-sm text-white/80" data-testid="queue-settings-summary">
                            {describe_queue_config(&current)}
                        </p>
                        <dl class="grid grid-cols-2 gap-x-6 gap-y-1 text-sm">
                            <dt class="text-white/40">"Parallel Task limit"</dt>
                            <dd class="text-white/80" data-testid="queue-settings-current-limit">
                                {current.parallel_task_limit}
                            </dd>
                            <dt class="text-white/40">"Auto-promote tasks"</dt>
                            <dd class="text-white/80" data-testid="queue-settings-current-auto-promote">
                                {on_off(current.auto_promote)}
                            </dd>
                            <dt class="text-white/40">"FIFO ordering"</dt>
                            <dd class="text-white/80">{on_off(current.fifo_ordering)}</dd>
                            <dt class="text-white/40">"CodeRabbit for AI review"</dt>
                            <dd class="text-white/80">{on_off(current.use_coderabbit)}</dd>
                        </dl>
                        <button
                            data-testid="queue-settings-edit"
                            class="px-4 py-2 rounded-lg bg-blue-500 hover:bg-blue-600 text-white font-medium transition-colors"
                            on:click=move |_| show_form.set(true)
                        >
                            "Edit Queue settings"
                        </button>
                    </div>
                    }}}
                }.into_any(),
                (false, Some(error)) => view! {
                    <div class="p-4 rounded-xl bg-red-500/10 border border-red-500/30 space-y-3" role="alert" data-testid="queue-settings-load-error">
                        <p class="text-sm text-red-400">{format!("Could not read the Queue settings: {error}")}</p>
                        <button
                            class="px-3 py-1.5 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                            on:click=move |_| load()
                        >
                            "Try again"
                        </button>
                    </div>
                }.into_any(),
                (false, None) => view! {
                    <p class="text-sm text-white/40">"Loading Queue settings..."</p>
                }.into_any(),
            }}

            <QueueSettingsModal show=show_form config=config />
        </div>
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}
