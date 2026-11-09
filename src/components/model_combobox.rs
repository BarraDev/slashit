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

/// The typed entry a blur commits. Text that still matches a listed row is a
/// search the person did not finish, not an ID, so blur commits nothing for it.
pub fn blur_entry(options: &[PickerOption], query: &str, refused_ids: &[String]) -> Option<PickerOption> {
    if !filter_indices(options, query).is_empty() {
        return None;
    }
    manual_option(options, query, refused_ids)
}

/// The identity of one row. A catalog row is its model ID, and the typed entry is
/// its own variant, so no row can take over another row's node when the list changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RowKey {
    Model(String),
    Typed(String),
}

impl RowKey {
    /// The value a choice of this row saves.
    pub fn value(&self) -> &str {
        match self {
            Self::Model(id) | Self::Typed(id) => id,
        }
    }
}

/// The rows offered for `query`, in display order: the typed entry first, then the
/// matching models. A catalog ID that repeats is offered once.
pub fn row_keys(options: &[PickerOption], query: &str, refused_ids: &[String]) -> Vec<RowKey> {
    let mut keys: Vec<RowKey> = manual_option(options, query, refused_ids)
        .map(|manual| RowKey::Typed(manual.id))
        .into_iter()
        .collect();
    for index in filter_indices(options, query) {
        let key = RowKey::Model(options[index].id.clone());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The offered row the keyboard highlight points to. `None` when there is no
/// highlight or when its row is no longer offered.
pub fn highlighted<'a>(keys: &'a [RowKey], active: Option<&RowKey>) -> Option<&'a RowKey> {
    let active = active?;
    keys.iter().find(|key| *key == active)
}

/// The row after (`down`) or before the highlight, wrapping at either end. A
/// highlight whose row is no longer offered counts as no highlight.
pub fn next_highlight(keys: &[RowKey], active: Option<&RowKey>, down: bool) -> Option<RowKey> {
    let count = keys.len();
    if count == 0 {
        return None;
    }
    let current = active.and_then(|active| keys.iter().position(|key| key == active));
    let index = match (current, down) {
        (None, true) => 0,
        (None, false) => count - 1,
        (Some(i), true) => (i + 1) % count,
        (Some(i), false) => (i + count - 1) % count,
    };
    Some(keys[index].clone())
}

/// The DOM ID of one row, derived from the component ID so several pickers on one
/// page do not share IDs. A model ID is escaped byte by byte, so distinct IDs never
/// share a DOM ID. `_` is always escaped, so the typed row's `_typed` cannot match one.
fn option_dom_id(prefix: &str, key: &RowKey) -> String {
    let slug = match key {
        RowKey::Model(id) => id
            .bytes()
            .map(|byte| match byte {
                b if b.is_ascii_alphanumeric() || b == b'-' => (b as char).to_string(),
                b => format!("_{b:02x}"),
            })
            .collect::<String>(),
        RowKey::Typed(_) => "_typed".to_string(),
    };
    format!("{prefix}-option-{slug}")
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

/// The name shown on a row.
fn row_name(options: &[PickerOption], key: &RowKey) -> String {
    match key {
        RowKey::Model(id) => saved_label(options, id),
        RowKey::Typed(typed) => format!("Use \"{typed}\" as typed"),
    }
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
    /// Runs discovery again. Only offered while `refreshable` is true.
    #[prop(into)]
    on_refresh: Callback<()>,
    /// Whether the runtime has a discovery to run again. Claude Code's aliases are
    /// fixed, so its picker has nothing to refresh.
    #[prop(into)]
    refreshable: Signal<bool>,
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
    // The highlighted row, by identity: it stays on the same model when the list
    // is filtered or refreshed.
    let (active, set_active) = signal(None::<RowKey>);
    let refused = StoredValue::new(refused_ids);
    let listbox_id = format!("{id}-listbox");
    let controls_id = listbox_id.clone();
    let option_prefix = StoredValue::new(id.clone());

    // The rows currently offered: a typed entry, then the matching options.
    let rows = Memo::new(move |_| row_keys(&options.get(), &query.get(), &refused.get_value()));

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
                let next = next_highlight(
                    &rows.get_untracked(),
                    active.get_untracked().as_ref(),
                    key == "ArrowDown",
                );
                set_active.set(next);
            }
            "Enter" if open.get() => {
                // Only a row the person moved to is chosen. Otherwise Enter
                // goes to the form, and a focused field never picks a row.
                let chosen = highlighted(&rows.get_untracked(), active.get_untracked().as_ref())
                    .map(|row| row.value().to_string());
                let Some(value) = chosen else {
                    return;
                };
                ev.prevent_default();
                choose(value);
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
                    aria-controls=move || open.get().then(|| controls_id.clone())
                    aria-expanded=move || if open.get() { "true" } else { "false" }
                    aria-activedescendant=move || match highlighted(&rows.get(), active.get().as_ref()) {
                        Some(row) if open.get() => option_dom_id(&option_prefix.get_value(), row),
                        _ => String::new(),
                    }
                    placeholder="Search or type a model ID"
                    disabled=move || disabled.get()
                    prop:value=move || if open.get() { query.get() } else { saved_label(&options.get(), &selected.get()) }
                    // Opened by a click, typing or an arrow key, not by focus alone: tabbing
                    // through the form must not drop a list over the footer's Save button.
                    on:click=move |_| {
                        set_open.set(true);
                        set_active.set(None);
                    }
                    on:blur=move |_| {
                        // A typed ID the person never chose is kept, not dropped on blur.
                        let typed = query.get_untracked();
                        if let Some(manual) = blur_entry(&options.get_untracked(), &typed, &refused.get_value()) {
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
                <Show when=move || refreshable.get()>
                    <button
                        type="button"
                        class="px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white/80 hover:bg-white/10 disabled:opacity-40"
                        disabled=move || disabled.get() || loading.get()
                        on:mousedown=move |ev: web_sys::MouseEvent| ev.prevent_default()
                        on:click=move |_| on_refresh.run(())
                    >
                        "Refresh"
                    </button>
                </Show>
            </div>

            <Show when=move || open.get()>
                <ul
                    id=listbox_id.clone()
                    role="listbox"
                    class="absolute z-50 w-full mt-1 py-1 rounded-lg bg-[#1a1a1a] border border-white/10 shadow-xl max-h-60 overflow-auto"
                >
                    <For
                        each=move || rows.get()
                        key=|row: &RowKey| row.clone()
                        children=move |row: RowKey| {
                            let dom_id = option_dom_id(&option_prefix.get_value(), &row);
                            let value = row.value().to_string();
                            let shown = value.clone();
                            let name_of = row.clone();
                            let is_active = Memo::new(move |_| active.get().as_ref() == Some(&row));
                            view! {
                                <li
                                    id=dom_id
                                    role="option"
                                    aria-selected=move || if is_active.get() { "true" } else { "false" }
                                    on:mousedown=move |ev: web_sys::MouseEvent| {
                                        ev.prevent_default();
                                        choose(value.clone());
                                    }
                                    class=move || format!(
                                        "px-3 py-2 cursor-pointer text-sm {}",
                                        if is_active.get() { "bg-blue-500/20 text-blue-300" } else { "text-white/80 hover:bg-white/10" }
                                    )
                                >
                                    <span class="block">{move || row_name(&options.get(), &name_of)}</span>
                                    <span class="block text-xs text-white/40">{shown}</span>
                                </li>
                            }
                        }
                    />

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
    fn a_blur_commits_a_typed_id_only_when_no_listed_row_matches() {
        let none: Vec<String> = Vec::new();
        assert!(blur_entry(&options(), "son", &none).is_none(), "a partial search is not an ID");
        assert!(blur_entry(&options(), "sonnet", &none).is_none(), "a listed ID is chosen from the list");
        assert_eq!(
            blur_entry(&options(), "custom/model", &none).map(|o| o.id),
            Some("custom/model".to_string()),
            "an ID no row matches is kept"
        );
    }

    #[test]
    fn a_refused_id_is_never_offered_as_a_typed_choice() {
        let refused = vec!["default".to_string()];
        assert!(manual_option(&options(), "default", &refused).is_none());
        assert!(manual_option(&options(), "default", &[]).is_some());
    }

    fn model(id: &str) -> RowKey {
        RowKey::Model(id.to_string())
    }

    #[test]
    fn a_highlight_follows_its_model_when_the_list_is_reordered() {
        // The highlight is on beam-free. The list is reordered and filtered
        // while it stays selected: the next arrow must move from beam-free itself.
        let before = vec![model("ollama/llama3.2:3b"), model("opencode-go/beam-free"), model("sonnet")];
        let after = vec![model("sonnet"), model("opencode-go/beam-free")];
        let active = model("opencode-go/beam-free");
        assert_eq!(highlighted(&before, Some(&active)), Some(&active));
        assert_eq!(next_highlight(&after, Some(&active), true), Some(model("sonnet")));
        assert_eq!(next_highlight(&after, Some(&active), false), Some(model("sonnet")));
    }

    #[test]
    fn a_highlight_whose_row_was_filtered_out_is_no_highlight() {
        let active = model("sonnet");
        let filtered = row_keys(&options(), "opencode-go/beam-free", &[]);
        assert_eq!(filtered, vec![model("opencode-go/beam-free")], "an exact ID is not offered again as typed");
        assert!(highlighted(&filtered, Some(&active)).is_none());
        assert_eq!(next_highlight(&filtered, Some(&active), true), Some(model("opencode-go/beam-free")));
    }

    #[test]
    fn arrows_wrap_and_an_empty_list_has_no_highlight() {
        let keys = vec![model("a"), model("b")];
        assert_eq!(next_highlight(&keys, Some(&model("b")), true), Some(model("a")));
        assert_eq!(next_highlight(&keys, Some(&model("a")), false), Some(model("b")));
        assert_eq!(next_highlight(&[], None, true), None);
    }

    #[test]
    fn enter_has_nothing_to_choose_without_a_highlight() {
        let keys = row_keys(&options(), "", &[]);
        assert!(highlighted(&keys, None).is_none());
    }

    #[test]
    fn a_typed_row_and_a_model_with_the_same_text_are_different_rows() {
        let typed = RowKey::Typed("sonnet-x".to_string());
        assert_ne!(typed, model("sonnet-x"));
        assert_ne!(option_dom_id("p", &typed), option_dom_id("p", &model("_typed")));
        assert_ne!(option_dom_id("p", &model("a/b")), option_dom_id("p", &model("a_2fb")));
    }

    #[test]
    fn a_catalog_id_repeated_by_the_runtime_is_offered_once() {
        let mut options = options();
        options.push(PickerOption::new("sonnet", "Duplicate", "claude"));
        let keys = row_keys(&options, "sonnet", &[]);
        assert_eq!(keys, vec![model("sonnet")]);
    }

    #[test]
    fn a_typed_row_comes_first_and_carries_the_typed_id() {
        let keys = row_keys(&options(), "custom/model", &[]);
        assert_eq!(keys, vec![RowKey::Typed("custom/model".to_string())]);
        assert_eq!(keys[0].value(), "custom/model");
    }
}
