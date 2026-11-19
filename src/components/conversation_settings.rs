// SPDX-License-Identifier: MPL-2.0
//! The Project Conversation send-key setting, shown in General settings.

use crate::components::toast;
use crate::models::SendShortcut;
use crate::services::conversation_service;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// A radio group for the send key. The choice is saved to `config.toml` when it
/// is made; if the save fails, the previous choice is restored and says so.
#[component]
pub fn ConversationSendSettings() -> impl IntoView {
    let (shortcut, set_shortcut) = signal(SendShortcut::default());
    spawn_local(async move {
        if let Ok(config) = conversation_service::get_conversations_config().await {
            let _ = set_shortcut.try_set(config.send_shortcut);
        }
    });

    let choose = move |choice: SendShortcut| {
        let previous = shortcut.get_untracked();
        set_shortcut.set(choice);
        spawn_local(async move {
            match conversation_service::update_conversations_config(choice).await {
                Ok(saved) => { let _ = set_shortcut.try_set(saved.send_shortcut); }
                Err(error) => {
                    let _ = set_shortcut.try_set(previous);
                    toast::error(format!("Could not save the send key; it was not changed. {error}"));
                }
            }
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
                    on:change=move |_| choose(SendShortcut::Enter)
                />
                <span class="text-sm text-white/80">"Enter. Shift+Enter adds a new line."</span>
            </label>
        </fieldset>
    }
}
