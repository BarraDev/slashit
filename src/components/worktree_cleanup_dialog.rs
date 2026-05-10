use leptos::prelude::*;
use leptos::task::spawn_local;
use uuid::Uuid;
use crate::services::{cleanup_worktree, update_task_status};
use crate::models::TaskStatus;

#[component]
pub fn WorktreeCleanupDialog(
    show: RwSignal<bool>,
    task_id: Uuid,
    task_title: String,
    on_complete: Callback<Uuid>,
) -> impl IntoView {
    let (cleaning, set_cleaning) = signal(false);
    let (error_msg, set_error_msg) = signal(None::<String>);

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| show.set(false)
                ></div>

                <div class="relative w-full max-w-md bg-[#0B0B0F] border border-amber-500/30 rounded-xl shadow-2xl" on:click=move |e| e.stop_propagation()>
                    <div class="flex items-center justify-between p-6 border-b border-white/5">
                        <div class="flex items-center gap-3">
                            <div class="w-10 h-10 rounded-lg bg-amber-500/20 flex items-center justify-center">
                                <svg class="w-5 h-5 text-amber-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
                                </svg>
                            </div>
                            <h2 class="text-lg font-semibold text-white/90">"Worktree Still Exists"</h2>
                        </div>
                        <button
                            on:click=move |_| show.set(false)
                            class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="p-6 space-y-3">
                        <p class="text-sm text-white/70">
                            "The task "
                            <strong class="text-white/90">{task_title.clone()}</strong>
                            " has an active worktree."
                        </p>
                        <p class="text-sm text-white/50">"What would you like to do?"</p>

                        {
                            move || {
                                if let Some(err) = error_msg.get() {
                                    view! {
                                        <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm">{err}</div>
                                    }.into_any()
                                } else {
                                    ().into_any()
                                }
                            }
                        }
                    </div>

                    <div class="flex items-center justify-end gap-3 p-6 border-t border-white/5">
                        <button
                            on:click=move |_| show.set(false)
                            class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                            disabled=move || cleaning.get()
                        >
                            "Cancel"
                        </button>
                        <button
                            class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                            on:click=move |_| {
                                let task_id = task_id;
                                let show = show;
                                let on_complete = on_complete;

                                spawn_local(async move {
                                    let _ = update_task_status(task_id.to_string(), TaskStatus::Done).await;
                                    on_complete.run(task_id);
                                    show.set(false);
                                });
                            }
                            disabled=move || cleaning.get()
                        >
                            "Skip & Complete"
                        </button>
                        <button
                            class="px-4 py-2 rounded-lg bg-blue-500 hover:bg-blue-600 text-white font-medium transition-colors disabled:bg-white/5 disabled:text-white/30"
                            on:click=move |_| {
                                let task_id = task_id;
                                let show = show;
                                let set_cleaning = set_cleaning;
                                let set_error_msg = set_error_msg;
                                let on_complete = on_complete;

                                set_cleaning.set(true);
                                set_error_msg.set(None);

                                spawn_local(async move {
                                    match cleanup_worktree(task_id.to_string()).await {
                                        Ok(_) => {
                                            match update_task_status(task_id.to_string(), TaskStatus::Done).await {
                                                Ok(_) => {
                                                    on_complete.run(task_id);
                                                    show.set(false);
                                                }
                                                Err(e) => {
                                                    set_error_msg.set(Some(format!("Failed to update task status: {}", e)));
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            set_error_msg.set(Some(format!("Failed to cleanup worktree: {}", e)));
                                        }
                                    }
                                    set_cleaning.set(false);
                                });
                            }
                            disabled=move || cleaning.get()
                        >
                            {move || if cleaning.get() { "Cleaning up..." } else { "Cleanup & Complete" } }
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
