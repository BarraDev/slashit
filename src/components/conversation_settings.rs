// SPDX-License-Identifier: MPL-2.0
//! The Project Conversation send-key setting, shown in General settings.

use crate::components::toast;
use crate::models::SendShortcut;
use crate::services::conversation_service;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// A radio group for the send key. The choice is saved to `config.toml` when it
/// is made. After a failed save the control shows what the file holds, and says so.
#[component]
pub fn ConversationSendSettings() -> impl IntoView {
    let (shortcut, set_shortcut) = signal(SendShortcut::default());
    // The choice is offered only once the stored value is known, and one save runs at a time.
    let (loaded, set_loaded) = signal(false);
    let (saving, set_saving) = signal(false);
    spawn_local(async move {
        if let Ok(config) = conversation_service::get_conversations_config().await {
            let _ = set_shortcut.try_set(config.send_shortcut);
        }
        let _ = set_loaded.try_set(true);
    });

    let choose = move |choice: SendShortcut| {
        if saving.get_untracked() || !loaded.get_untracked() {
            return;
        }
        set_saving.set(true);
        set_shortcut.set(choice);
        spawn_local(async move {
            match conversation_service::update_conversations_config(choice).await {
                Ok(saved) => { let _ = set_shortcut.try_set(saved.send_shortcut); }
                Err(error) => {
                    // The save may have reached the file before the error. Show what the file holds.
                    if let Ok(config) = conversation_service::get_conversations_config().await {
                        let _ = set_shortcut.try_set(config.send_shortcut);
                    }
                    toast::error(format!("The send key could not be confirmed as saved. The choice shown is the one the settings file holds. {error}"));
                }
            }
            let _ = set_saving.try_set(false);
        });
    };

    view! {
        <fieldset data-testid="conversation-send-settings" class="p-4 rounded-xl bg-white/5 border border-white/10">
            <legend class="font-medium text-white/90 px-1">"Send a Conversation message with"</legend>
            <p class="text-sm text-white/40 mt-1 mb-3">"The shortcut is shown under the message box. Messages are never sent while an input method is composing text."</p>
            <label class="flex items-start gap-3 py-2 cursor-pointer">
                <input
                    type="radio"
                    name="conversation-send-shortcut"
                    data-testid="conversation-send-modifier-enter"
                    class="mt-1"
                    prop:checked=move || shortcut.get() == SendShortcut::ModifierEnter
                    prop:disabled=move || !loaded.get() || saving.get()
                    on:change=move |_| choose(SendShortcut::ModifierEnter)
                />
                <span class="text-sm text-white/80">"Ctrl+Enter (Cmd+Enter on macOS). Enter adds a new line. This is the default."</span>
            </label>
            <label class="flex items-start gap-3 py-2 cursor-pointer">
                <input
                    type="radio"
                    name="conversation-send-shortcut"
                    data-testid="conversation-send-enter"
                    class="mt-1"
                    prop:checked=move || shortcut.get() == SendShortcut::Enter
                    prop:disabled=move || !loaded.get() || saving.get()
                    on:change=move |_| choose(SendShortcut::Enter)
                />
                <span class="text-sm text-white/80">"Enter. Shift+Enter adds a new line."</span>
            </label>
        </fieldset>
    }
}
