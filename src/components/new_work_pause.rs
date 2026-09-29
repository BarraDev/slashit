//! Why queued work is not starting, when the reason is disk space.
//!
//! The backend decides; this only shows it. A fresh reading is asked for
//! while the board is open, so the notice appears and clears on its own as
//! space runs out or comes back. It is status, never a prompt: there is
//! nothing here that deletes or cleans anything.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::models::storage_usage::StartBlock;
use crate::services::storage_usage_service::get_new_work_pause;

/// How often the board asks. The reading is one `statvfs`, and the
/// scheduler itself asks every three seconds.
const POLL_MS: u32 = 5_000;

/// Whether new task executions are paused, for anything on the board.
#[derive(Clone, Copy)]
pub struct NewWorkPause(pub RwSignal<Option<StartBlock>>);

/// Keep a [`NewWorkPause`] current for as long as the calling component is
/// mounted, and provide it to its children.
pub fn provide_new_work_pause() -> NewWorkPause {
    let pause = RwSignal::new(None::<StartBlock>);
    let ask = move || {
        spawn_local(async move {
            // A failed request leaves the last answer: the backend refuses a
            // start on its own either way.
            if let Ok(answer) = get_new_work_pause().await {
                if pause.try_get_untracked().is_some_and(|current| current != answer) {
                    pause.try_set(answer);
                }
            }
        })
    };
    ask();
    let ticker = StoredValue::new_local(Some(gloo_timers::callback::Interval::new(POLL_MS, ask)));
    on_cleanup(move || ticker.dispose());
    let pause = NewWorkPause(pause);
    provide_context(pause);
    pause
}

/// The notice, shown only while new work is paused.
#[component]
pub fn NewWorkPauseNotice(testid: &'static str) -> impl IntoView {
    let pause = use_context::<NewWorkPause>();
    move || {
        pause.and_then(|NewWorkPause(pause)| pause.get()).map(|block| {
            view! {
                <div
                    data-testid=testid
                    role="status"
                    class="mt-2 rounded-md border border-red-500/30 bg-red-500/10 px-2 py-1.5"
                >
                    <p class="text-xs font-medium text-red-200">{block.headline()}</p>
                    <p class="text-xs text-red-200/70 mt-0.5">{block.detail()}</p>
                </div>
            }
        })
    }
}
