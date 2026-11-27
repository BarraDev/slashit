// SPDX-License-Identifier: MPL-2.0
//! How the Tasks page presents the board: as columns or as a list, and with
//! detailed or compact cards. Both choices are presentation only and are kept
//! in the webview's local storage, so they survive a reload.

use leptos::prelude::*;

const LAYOUT_KEY: &str = "slashit_tasks_layout";
const DENSITY_KEY: &str = "slashit_tasks_card_density";

/// Columns side by side, or the same columns stacked as one list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BoardLayout {
    #[default]
    Board,
    List,
}

impl BoardLayout {
    fn as_str(self) -> &'static str {
        match self {
            BoardLayout::Board => "board",
            BoardLayout::List => "list",
        }
    }

    /// An unknown or missing value is the default, never an error.
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("list") => BoardLayout::List,
            _ => BoardLayout::Board,
        }
    }
}

/// Every card detail, or only the title, status and priority on one line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CardDensity {
    #[default]
    Detailed,
    Compact,
}

impl CardDensity {
    fn as_str(self) -> &'static str {
        match self {
            CardDensity::Detailed => "detailed",
            CardDensity::Compact => "compact",
        }
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("compact") => CardDensity::Compact,
            _ => CardDensity::Detailed,
        }
    }
}

/// Where the choices are kept. The webview's local storage in the app; a map
/// in tests.
pub trait PrefStore {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
}

/// The webview's local storage. Without one (or when it refuses a write) the
/// choice lasts only for this page.
pub struct LocalPrefStore;

impl PrefStore for LocalPrefStore {
    fn get(&self, key: &str) -> Option<String> {
        web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .and_then(|s| s.get_item(key).ok().flatten())
    }

    fn set(&self, key: &str, value: &str) {
        if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = storage.set_item(key, value);
        }
    }
}

pub fn load_layout(store: &impl PrefStore) -> BoardLayout {
    BoardLayout::parse(store.get(LAYOUT_KEY).as_deref())
}

pub fn save_layout(store: &impl PrefStore, layout: BoardLayout) {
    store.set(LAYOUT_KEY, layout.as_str());
}

pub fn load_density(store: &impl PrefStore) -> CardDensity {
    CardDensity::parse(store.get(DENSITY_KEY).as_deref())
}

pub fn save_density(store: &impl PrefStore, density: CardDensity) {
    store.set(DENSITY_KEY, density.as_str());
}

/// The board's current presentation, shared with its columns and cards.
#[derive(Clone, Copy)]
pub struct BoardPrefs {
    pub layout: RwSignal<BoardLayout>,
    pub density: RwSignal<CardDensity>,
}

impl BoardPrefs {
    /// The choices last saved, or the defaults.
    pub fn load() -> Self {
        Self {
            layout: RwSignal::new(load_layout(&LocalPrefStore)),
            density: RwSignal::new(load_density(&LocalPrefStore)),
        }
    }

    pub fn set_layout(self, layout: BoardLayout) {
        self.layout.set(layout);
        save_layout(&LocalPrefStore, layout);
    }

    pub fn set_density(self, density: CardDensity) {
        self.density.set(density);
        save_density(&LocalPrefStore, density);
    }
}

/// The Board/List and Detailed/Compact switches in the board's header.
#[component]
pub fn BoardPrefsToggle(prefs: BoardPrefs) -> impl IntoView {
    let segment = |active: bool| {
        if active {
            "px-2.5 py-1 text-xs font-medium rounded-md bg-white/10 text-white/90"
        } else {
            "px-2.5 py-1 text-xs font-medium rounded-md text-white/40 hover:text-white/70"
        }
    };
    view! {
        <div class="flex items-center gap-2">
            <div role="group" aria-label="Tasks view" class="flex items-center gap-0.5 p-0.5 rounded-lg bg-white/5">
                <button
                    type="button"
                    data-testid="tasks-view-board"
                    aria-pressed=move || (prefs.layout.get() == BoardLayout::Board).to_string()
                    class=move || segment(prefs.layout.get() == BoardLayout::Board)
                    on:click=move |_| prefs.set_layout(BoardLayout::Board)
                >
                    "Board"
                </button>
                <button
                    type="button"
                    data-testid="tasks-view-list"
                    aria-pressed=move || (prefs.layout.get() == BoardLayout::List).to_string()
                    class=move || segment(prefs.layout.get() == BoardLayout::List)
                    on:click=move |_| prefs.set_layout(BoardLayout::List)
                >
                    "List"
                </button>
            </div>
            <div role="group" aria-label="Card density" class="flex items-center gap-0.5 p-0.5 rounded-lg bg-white/5">
                <button
                    type="button"
                    data-testid="tasks-density-detailed"
                    aria-pressed=move || (prefs.density.get() == CardDensity::Detailed).to_string()
                    class=move || segment(prefs.density.get() == CardDensity::Detailed)
                    on:click=move |_| prefs.set_density(CardDensity::Detailed)
                >
                    "Detailed"
                </button>
                <button
                    type="button"
                    data-testid="tasks-density-compact"
                    aria-pressed=move || (prefs.density.get() == CardDensity::Compact).to_string()
                    class=move || segment(prefs.density.get() == CardDensity::Compact)
                    on:click=move |_| prefs.set_density(CardDensity::Compact)
                >
                    "Compact"
                </button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct MemoryStore(RefCell<HashMap<String, String>>);

    impl PrefStore for MemoryStore {
        fn get(&self, key: &str) -> Option<String> {
            self.0.borrow().get(key).cloned()
        }
        fn set(&self, key: &str, value: &str) {
            self.0.borrow_mut().insert(key.to_string(), value.to_string());
        }
    }

    #[test]
    fn with_nothing_saved_the_board_shows_detailed_columns() {
        let store = MemoryStore::default();
        assert_eq!(load_layout(&store), BoardLayout::Board);
        assert_eq!(load_density(&store), CardDensity::Detailed);
    }

    #[test]
    fn a_saved_list_view_is_what_the_next_load_reads() {
        let store = MemoryStore::default();
        save_layout(&store, BoardLayout::List);
        assert_eq!(load_layout(&store), BoardLayout::List);
        save_layout(&store, BoardLayout::Board);
        assert_eq!(load_layout(&store), BoardLayout::Board);
    }

    #[test]
    fn a_saved_compact_density_is_what_the_next_load_reads() {
        let store = MemoryStore::default();
        save_density(&store, CardDensity::Compact);
        assert_eq!(load_density(&store), CardDensity::Compact);
        save_density(&store, CardDensity::Detailed);
        assert_eq!(load_density(&store), CardDensity::Detailed);
    }

    #[test]
    fn the_two_choices_are_kept_apart() {
        let store = MemoryStore::default();
        save_layout(&store, BoardLayout::List);
        assert_eq!(load_density(&store), CardDensity::Detailed);
        save_density(&store, CardDensity::Compact);
        assert_eq!(load_layout(&store), BoardLayout::List);
    }

    #[test]
    fn an_unrecognised_saved_value_falls_back_to_the_default() {
        let store = MemoryStore::default();
        store.set(LAYOUT_KEY, "grid");
        store.set(DENSITY_KEY, "");
        assert_eq!(load_layout(&store), BoardLayout::Board);
        assert_eq!(load_density(&store), CardDensity::Detailed);
    }
}
