// SPDX-License-Identifier: MPL-2.0
//! A searchable model picker with a typed fallback. It shows the models the
//! runtime lists, filters them by ID, name and provider, and lets a person
//! type any ID. A saved model that the list does not contain stays selected
//! and is labelled as such, so a failed discovery never clears it.

use leptos::callback::Callback;
use leptos::prelude::*;

/// One selectable model.
#[derive(Clone, Debug, PartialEq)]
pub struct PickerOption {
    /// The value saved on the Task. An empty ID means "no choice".
    pub id: String,
    /// The label shown in the list.
    pub name: String,
    /// The runtime that lists the model, for example `opencode` or `claude`.
    pub provider: String,
}

impl PickerOption {
    pub fn new(id: impl Into<String>, name: impl Into<String>, provider: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            provider: provider.into(),
        }
    }
}

/// Decides which load may update a picker. Each load takes a token and only the
/// newest token is accepted, so a response for an earlier runtime, or for an
/// overlapping refresh, never replaces the list that is current.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadGate {
    latest: u64,
}

impl LoadGate {
    /// Starts a load and returns its token. Every earlier token is now stale.
    pub fn begin(&mut self) -> u64 {
        self.latest += 1;
        self.latest
    }

    /// Whether a response for `token` may update the picker.
    pub fn accepts(&self, token: u64) -> bool {
        token == self.latest
    }
}

/// The loaded list for one picker. A failed load keeps the options it had, so
/// a refresh that fails does not empty the picker. Switching runtime clears the
/// rows, so no list is shown under a runtime it was not discovered for.
#[derive(Clone, Copy)]
pub struct PickerState {
    pub options: RwSignal<Vec<PickerOption>>,
    pub error: RwSignal<Option<String>>,
    pub loading: RwSignal<bool>,
    runtime: RwSignal<&'static str>,
    gate: RwSignal<LoadGate>,
}

impl PickerState {
    pub fn new() -> Self {
        Self {
            options: RwSignal::new(Vec::new()),
            error: RwSignal::new(None),
            loading: RwSignal::new(false),
            runtime: RwSignal::new(""),
            gate: RwSignal::new(LoadGate::default()),
        }
    }

    /// Loads `provider`'s list (`claude` or `opencode`). `refresh` runs the
    /// runtime's discovery again.
    pub fn load(self, provider: &'static str, refresh: bool) {
        if self.runtime.get_untracked() != provider {
            self.options.set(Vec::new());
            self.error.set(None);
            self.runtime.set(provider);
        }
        let mut token = 0;
        self.gate.update(|gate| token = gate.begin());
        self.loading.set(true);
        leptos::task::spawn_local(async move {
            let result = crate::services::agent_service::list_runtime_models(provider, refresh).await;
            if !self.gate.get_untracked().accepts(token) {
                return;
            }
            match result {
                Ok(catalog) => {
                    self.options.set(
                        catalog
                            .models
                            .into_iter()
                            .map(|model| PickerOption::new(model.id, model.name, provider))
                            .collect(),
                    );
                    self.error.set(catalog.error);
                }
                Err(reason) => self.error.set(Some(reason)),
            }
            self.loading.set(false);
        });
    }
}

/// Indices of the options whose ID, name or provider contains `query`, ignoring
/// case. An empty query keeps every option, in its original order.
pub fn filter_indices(options: &[PickerOption], query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    options
        .iter()
        .enumerate()
        .filter(|(_, option)| {
            needle.is_empty()
                || option.id.to_lowercase().contains(&needle)
                || option.name.to_lowercase().contains(&needle)
                || option.provider.to_lowercase().contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

/// The entry offered for a typed ID that no option names. `None` when the
/// query is empty, when an option already has that ID, or when the ID is
/// refused for this field (for example `default` for the AI Reviewer).
pub fn manual_option(
    options: &[PickerOption],
    query: &str,
    refused_ids: &[String],
) -> Option<PickerOption> {
    let typed = query.trim();
    if typed.is_empty()
        || typed.chars().any(char::is_whitespace)
        || options.iter().any(|option| option.id == typed)
        || refused_ids.iter().any(|refused| refused == typed)
    {
        return None;
    }
    Some(PickerOption::new(typed, format!("Use \"{typed}\" as typed"), "typed"))
}

/// The DOM ID of one row, derived from the component ID so several pickers on
/// one page do not share IDs.
fn option_dom_id(prefix: &str, index: usize) -> String {
    format!("{prefix}-option-{index}")
}

/// The label shown for the saved value, or the placeholder when none is saved.
fn saved_label(options: &[PickerOption], selected: &str) -> String {
    if selected.is_empty() {
        return String::new();
    }
    options
        .iter()
        .find(|option| option.id == selected)
        .map(|option| option.name.clone())
        .unwrap_or_else(|| selected.to_string())
}

#[component]
pub fn ModelCombobox(
    /// Used for the element IDs and the label association.
    #[prop(into)]
    id: String,
    /// The models to list. Empty when no list has loaded.
    #[prop(into)]
    options: Signal<Vec<PickerOption>>,
    /// The saved value. Shown even when `options` does not contain it.
    #[prop(into)]
    selected: Signal<String>,
    /// Receives the chosen ID.
    #[prop(into)]
    on_change: Callback<String>,
    /// Runs discovery again.
    #[prop(into)]
    on_refresh: Callback<()>,
    #[prop(into)]
    loading: Signal<bool>,
    /// The reason the list is not fresh, when it is not.
    #[prop(into)]
    error: Signal<Option<String>>,
    #[prop(into)]
    disabled: Signal<bool>,
    /// IDs that a typed entry may not choose.
    #[prop(default = Vec::new())]
    refused_ids: Vec<String>,
) -> impl IntoView {
    let (open, set_open) = signal(false);
    let (query, set_query) = signal(String::new());
    // The highlighted row, once the person has moved to one with the arrow keys.
    let (active, set_active) = signal(None::<usize>);
    let refused = StoredValue::new(refused_ids);
    let listbox_id = format!("{id}-listbox");
    let option_prefix = StoredValue::new(id.clone());

    // The rows currently offered: the matching options, then a typed entry.
    let rows = move || {
        let all = options.get();
        let text = query.get();
        let mut rows: Vec<PickerOption> = filter_indices(&all, &text)
            .into_iter()
            .map(|index| all[index].clone())
            .collect();
        if let Some(manual) = manual_option(&all, &text, &refused.get_value()) {
            rows.insert(0, manual);
        }
        rows
    };

    let choose = move |chosen: String| {
        on_change.run(chosen);
        set_open.set(false);
        set_query.set(String::new());
        set_active.set(None);
    };

    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        let key = ev.key();
        match key.as_str() {
            "ArrowDown" | "ArrowUp" => {
                ev.prevent_default();
                set_open.set(true);
                let count = rows().len();
                if count > 0 {
                    let down = key == "ArrowDown";
                    set_active.update(|i| {
                        *i = Some(match *i {
                            None if down => 0,
                            None => count - 1,
                            Some(current) if down => (current + 1) % count,
                            Some(current) => (current + count - 1) % count,
                        });
                    });
                }
            }
            "Enter" if open.get() => {
                // Only a row the person moved to is chosen. Otherwise Enter
                // goes to the form, and a focused field never picks a row.
                let Some(row) = active.get().and_then(|i| rows().get(i).cloned()) else {
                    return;
                };
                ev.prevent_default();
                choose(row.id);
            }
            "Escape" if open.get() => {
                ev.prevent_default();
                set_open.set(false);
                set_query.set(String::new());
            }
            _ => {}
        }
    };

    view! {
        <div class="relative">
            <div class="flex gap-2">
                <input
                    id=id.clone()
                    type="text"
                    role="combobox"
                    autocomplete="off"
                    aria-autocomplete="list"
                    aria-controls=listbox_id.clone()
                    aria-expanded=move || if open.get() { "true" } else { "false" }
                    aria-activedescendant=move || match active.get() {
                        Some(index) if open.get() => option_dom_id(&option_prefix.get_value(), index),
                        _ => String::new(),
                    }
                    placeholder="Search or type a model ID"
                    disabled=move || disabled.get()
                    prop:value=move || if open.get() { query.get() } else { saved_label(&options.get(), &selected.get()) }
                    on:focus=move |_| {
                        set_open.set(true);
                        set_active.set(None);
                    }
                    on:blur=move |_| {
                        // A typed ID the person never chose is kept, not dropped on blur.
                        let typed = query.get_untracked();
                        if let Some(manual) = manual_option(&options.get_untracked(), &typed, &refused.get_value()) {
                            choose(manual.id);
                        } else {
                            set_open.set(false);
                            set_query.set(String::new());
                        }
                    }
                    on:input=move |ev| {
                        set_query.set(event_target_value(&ev));
                        set_open.set(true);
                        set_active.set(None);
                    }
                    on:keydown=on_keydown
                    class="flex-1 min-w-0 px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                />
                <button
                    type="button"
                    class="px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white/80 hover:bg-white/10 disabled:opacity-40"
                    disabled=move || disabled.get() || loading.get()
                    on:mousedown=move |ev: web_sys::MouseEvent| ev.prevent_default()
                    on:click=move |_| {
                        // A refresh can reorder the rows, so no earlier highlight carries over.
                        set_active.set(None);
                        on_refresh.run(());
                    }
                >
                    "Refresh"
                </button>
            </div>

            <Show when=move || open.get()>
                <ul
                    id=listbox_id.clone()
                    role="listbox"
                    class="absolute z-50 w-full mt-1 py-1 rounded-lg bg-[#1a1a1a] border border-white/10 shadow-xl max-h-60 overflow-auto"
                >
                    {move || rows().into_iter().enumerate().map(|(index, row)| {
                        let chosen = row.id.clone();
                        let is_active = move || active.get() == Some(index);
                        let dom_id = option_dom_id(&option_prefix.get_value(), index);
                        view! {
                            <li
                                id=dom_id
                                role="option"
                                aria-selected=move || if is_active() { "true" } else { "false" }
                                on:mousedown=move |ev: web_sys::MouseEvent| {
                                    ev.prevent_default();
                                    choose(chosen.clone());
                                }
                                class=move || format!(
                                    "px-3 py-2 cursor-pointer text-sm {}",
                                    if is_active() { "bg-blue-500/20 text-blue-300" } else { "text-white/80 hover:bg-white/10" }
                                )
                            >
                                <span class="block">{row.name.clone()}</span>
                                <span class="block text-xs text-white/40">{row.id.clone()}</span>
                            </li>
                        }
                    }).collect::<Vec<_>>()}
                </ul>
            </Show>

            <p class="mt-1.5 text-xs text-white/50" aria-live="polite">
                {move || {
                    if loading.get() {
                        "Loading models…".to_string()
                    } else if let Some(reason) = error.get() {
                        format!("Could not refresh the list: {reason}. Saved values are kept.")
                    } else {
                        let all = options.get();
                        let current = selected.get();
                        if !current.is_empty() && !all.iter().any(|o| o.id == current) {
                            format!("{current} is not in the current list. It stays selected until you change it.")
                        } else if all.is_empty() {
                            "No models are listed. Type a model ID.".to_string()
                        } else {
                            String::new()
                        }
                    }
                }}
            </p>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<PickerOption> {
        vec![
            PickerOption::new("ollama/llama3.2:3b", "ollama/llama3.2:3b", "opencode"),
            PickerOption::new("opencode-go/beam-free", "opencode-go/beam-free", "opencode"),
            PickerOption::new("sonnet", "Claude Sonnet (latest alias)", "claude"),
        ]
    }

    #[test]
    fn an_empty_query_keeps_every_option_in_order() {
        assert_eq!(filter_indices(&options(), ""), vec![0, 1, 2]);
        assert_eq!(filter_indices(&options(), "   "), vec![0, 1, 2]);
    }

    #[test]
    fn the_query_matches_id_name_or_provider_ignoring_case() {
        assert_eq!(filter_indices(&options(), "BEAM"), vec![1]);
        assert_eq!(filter_indices(&options(), "latest"), vec![2]);
        assert_eq!(filter_indices(&options(), "claude"), vec![2]);
        assert_eq!(filter_indices(&options(), "opencode"), vec![0, 1]);
        assert!(filter_indices(&options(), "gemini").is_empty());
    }

    #[test]
    fn a_typed_id_is_offered_unless_an_option_already_has_it() {
        let typed = manual_option(&options(), "custom/model", &[]).expect("offered");
        assert_eq!(typed.id, "custom/model");
        assert!(manual_option(&options(), "sonnet", &[]).is_none());
        assert!(manual_option(&options(), "  ", &[]).is_none());
        assert!(manual_option(&options(), "two words", &[]).is_none());
    }

    #[test]
    fn only_the_newest_load_may_update_the_picker() {
        let mut gate = LoadGate::default();
        let first = gate.begin(); // a provider switch
        let second = gate.begin(); // a provider switch back, or an overlapping refresh
        assert!(!gate.accepts(first), "the earlier response arrives last and is dropped");
        assert!(gate.accepts(second));
    }

    #[test]
    fn an_overlapping_refresh_supersedes_the_one_before_it() {
        let mut gate = LoadGate::default();
        let a = gate.begin();
        let b = gate.begin();
        let c = gate.begin();
        assert!(!gate.accepts(a));
        assert!(!gate.accepts(b));
        assert!(gate.accepts(c));
    }

    #[test]
    fn a_response_for_the_current_token_is_accepted_when_it_arrives_first() {
        let mut gate = LoadGate::default();
        let only = gate.begin();
        assert!(gate.accepts(only));
    }

    #[test]
    fn a_saved_model_missing_from_the_list_is_still_shown_as_saved() {
        // A failed refresh leaves the list without the saved model; its ID is shown.
        assert_eq!(saved_label(&options(), "ollama/gone:1"), "ollama/gone:1");
        assert_eq!(saved_label(&options(), "sonnet"), "Claude Sonnet (latest alias)");
        assert_eq!(saved_label(&options(), ""), "");
    }

    #[test]
    fn a_refused_id_is_never_offered_as_a_typed_choice() {
        let refused = vec!["default".to_string()];
        assert!(manual_option(&options(), "default", &refused).is_none());
        assert!(manual_option(&options(), "default", &[]).is_some());
    }
}
