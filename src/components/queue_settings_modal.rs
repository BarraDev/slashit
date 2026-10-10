use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::models::QueueConfig;
use crate::services::update_queue_config;

/// The bounds the backend enforces on the parallel Task limit.
pub const MIN_PARALLEL_TASK_LIMIT: u32 = 1;
pub const MAX_PARALLEL_TASK_LIMIT: u32 = 10;

/// What a limit field's text means: a whole number, held within the bounds.
/// `None` for text that is not a number (an empty box mid-edit), which leaves
/// the previous value in place.
fn parse_parallel_limit(text: &str) -> Option<u32> {
    text.trim()
        .parse::<u32>()
        .ok()
        .map(|limit| limit.clamp(MIN_PARALLEL_TASK_LIMIT, MAX_PARALLEL_TASK_LIMIT))
}

/// One line saying how the Queue behaves under `config`, for places that
/// show the settings without editing them.
pub fn describe_queue_config(config: &QueueConfig) -> String {
    let promotion = if config.auto_promote {
        "Queued Tasks start automatically"
    } else {
        "Queued Tasks wait until promotion is turned on"
    };
    format!(
        "{promotion}, up to {} at a time",
        config.parallel_task_limit
    )
}

/// The form for the Queue settings.
///
/// `config` is the settings the backend is running under. The form shows them
/// every time it opens (an abandoned edit does not linger) and, after a save,
/// replaces them with what the backend answers.
#[component]
pub fn QueueSettingsModal(show: RwSignal<bool>, config: RwSignal<QueueConfig>) -> impl IntoView {
    let (parallel_limit, set_parallel_limit) = signal(config.get_untracked().parallel_task_limit);
    let (auto_promote, set_auto_promote) = signal(config.get_untracked().auto_promote);
    let (fifo_ordering, set_fifo_ordering) = signal(config.get_untracked().fifo_ordering);
    let (use_coderabbit, set_use_coderabbit) = signal(config.get_untracked().use_coderabbit);
    let (saving, set_saving) = signal(false);
    let (error_msg, set_error_msg) = signal(None::<String>);

    Effect::new(move |_| {
        if show.get() {
            let current = config.get_untracked();
            set_parallel_limit.set(current.parallel_task_limit);
            set_auto_promote.set(current.auto_promote);
            set_fifo_ordering.set(current.fifo_ordering);
            set_use_coderabbit.set(current.use_coderabbit);
            set_error_msg.set(None);
        }
    });

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| show.set(false)
                ></div>

                <div data-testid="queue-settings-modal" class="relative w-full max-w-lg bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl" on:click=move |e| e.stop_propagation()>
                    <div class="flex items-center justify-between p-6 border-b border-white/5">
                        <h2 class="text-lg font-semibold text-white/90">"Queue Settings"</h2>
                        <button
                            on:click=move |_| show.set(false)
                            class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                        >
                            <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                            </svg>
                        </button>
                    </div>

                    <div class="p-6 space-y-5">
                        <div>
                            <label for="parallel-limit" class="block text-sm font-medium text-white/70 mb-1.5">"Parallel Task Limit"</label>
                            <input
                                id="parallel-limit"
                                data-testid="queue-settings-parallel-limit"
                                type="number"
                                min=MIN_PARALLEL_TASK_LIMIT
                                max=MAX_PARALLEL_TASK_LIMIT
                                prop:value=move || parallel_limit.get().to_string()
                                on:input=move |ev| {
                                    if let Some(limit) = parse_parallel_limit(&event_target_value(&ev)) {
                                        set_parallel_limit.set(limit);
                                    }
                                }
                                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                            />
                            <p class="mt-1 text-xs text-white/40">{format!("Maximum number of tasks that can run simultaneously ({MIN_PARALLEL_TASK_LIMIT}-{MAX_PARALLEL_TASK_LIMIT})")}</p>
                        </div>

                        <label class="flex items-center gap-3 p-4 rounded-xl bg-white/5 border border-white/10 cursor-pointer hover:bg-white/[0.07] transition-colors">
                            <input
                                type="checkbox"
                                data-testid="queue-settings-auto-promote"
                                prop:checked=move || auto_promote.get()
                                on:change=move |ev| {
                                    set_auto_promote.set(event_target_checked(&ev));
                                }
                                class="w-5 h-5 rounded-md bg-white/10 border-white/20 text-blue-500 focus:ring-blue-500/50 focus:ring-offset-0"
                            />
                            <div>
                                <div class="text-sm font-medium text-white/80">"Auto-promote tasks"</div>
                                <div class="text-xs text-white/40">"Automatically promote tasks from Queue to In Progress when capacity is available"</div>
                            </div>
                        </label>

                        <label class="flex items-center gap-3 p-4 rounded-xl bg-white/5 border border-white/10 cursor-pointer hover:bg-white/[0.07] transition-colors">
                            <input
                                type="checkbox"
                                data-testid="queue-settings-fifo-ordering"
                                prop:checked=move || fifo_ordering.get()
                                on:change=move |ev| {
                                    set_fifo_ordering.set(event_target_checked(&ev));
                                }
                                class="w-5 h-5 rounded-md bg-white/10 border-white/20 text-blue-500 focus:ring-blue-500/50 focus:ring-offset-0"
                            />
                            <div>
                                <div class="text-sm font-medium text-white/80">"FIFO ordering"</div>
                                <div class="text-xs text-white/40">"Process queued tasks in First-In-First-Out order"</div>
                            </div>
                        </label>

                        <label class="flex items-center gap-3 p-4 rounded-xl bg-white/5 border border-white/10 cursor-pointer hover:bg-white/[0.07] transition-colors">
                            <input
                                type="checkbox"
                                data-testid="queue-settings-use-coderabbit"
                                prop:checked=move || use_coderabbit.get()
                                on:change=move |ev| {
                                    set_use_coderabbit.set(event_target_checked(&ev));
                                }
                                class="w-5 h-5 rounded-md bg-white/10 border-white/20 text-blue-500 focus:ring-blue-500/50 focus:ring-offset-0"
                            />
                            <div>
                                <div class="text-sm font-medium text-white/80">"Use CodeRabbit for AI review"</div>
                                <div class="text-xs text-white/40">"Run CodeRabbit CLI review in parallel with Claude during AI review phase (requires coderabbit CLI)"</div>
                            </div>
                        </label>

                        {
                            move || {
                                if let Some(err) = error_msg.get() {
                                    view! {
                                        <div role="alert" data-testid="queue-settings-error" class="p-3 rounded-lg bg-red-500/10 border border-red-500/30 text-red-400 text-sm">{err}</div>
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
                            disabled=move || saving.get()
                        >
                            "Cancel"
                        </button>
                        <button
                            data-testid="queue-settings-save"
                            class="px-4 py-2 rounded-lg bg-blue-500 hover:bg-blue-600 text-white font-medium transition-colors disabled:bg-white/5 disabled:text-white/30"
                            on:click=on_save(SaveCtx {
                                config,
                                parallel_limit: parallel_limit.into(),
                                auto_promote: auto_promote.into(),
                                fifo_ordering: fifo_ordering.into(),
                                use_coderabbit: use_coderabbit.into(),
                                saving: set_saving,
                                error_msg: set_error_msg,
                                show,
                            })
                            disabled=move || saving.get()
                        >
                            {move || if saving.get() { "Saving..." } else { "Save" } }
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

struct SaveCtx {
    config: RwSignal<QueueConfig>,
    parallel_limit: Signal<u32>,
    auto_promote: Signal<bool>,
    fifo_ordering: Signal<bool>,
    use_coderabbit: Signal<bool>,
    saving: WriteSignal<bool>,
    error_msg: WriteSignal<Option<String>>,
    show: RwSignal<bool>,
}

fn on_save(ctx: SaveCtx) -> impl Fn(leptos::ev::MouseEvent) + Clone {
    move |_| {
        let parallel_limit = ctx.parallel_limit;
        let auto_promote = ctx.auto_promote;
        let fifo_ordering = ctx.fifo_ordering;
        let use_coderabbit = ctx.use_coderabbit;
        let config = ctx.config;
        let saving = ctx.saving;
        let error_msg = ctx.error_msg;
        let show = ctx.show;
        spawn_local(async move {
            let new_config = QueueConfig {
                parallel_task_limit: parallel_limit.get(),
                auto_promote: auto_promote.get(),
                fifo_ordering: fifo_ordering.get(),
                use_coderabbit: use_coderabbit.get(),
            };

            saving.set(true);
            error_msg.set(None);

            match update_queue_config(new_config).await {
                Ok(saved) => {
                    config.set(saved);
                    show.set(false);
                }
                Err(e) => {
                    error_msg.set(Some(e));
                }
            }
            saving.set(false);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_is_a_whole_number_held_within_the_bounds() {
        assert_eq!(parse_parallel_limit("4"), Some(4));
        assert_eq!(parse_parallel_limit(" 4 "), Some(4));
        assert_eq!(parse_parallel_limit("0"), Some(MIN_PARALLEL_TASK_LIMIT));
        assert_eq!(parse_parallel_limit("99"), Some(MAX_PARALLEL_TASK_LIMIT));
    }

    #[test]
    fn text_that_is_not_a_number_leaves_the_limit_alone() {
        assert_eq!(parse_parallel_limit(""), None);
        assert_eq!(parse_parallel_limit("-2"), None);
        assert_eq!(parse_parallel_limit("two"), None);
    }

    #[test]
    fn the_summary_says_whether_queued_tasks_start_by_themselves() {
        let on = describe_queue_config(&QueueConfig::default());
        assert_eq!(on, "Queued Tasks start automatically, up to 3 at a time");

        let off = describe_queue_config(&QueueConfig {
            auto_promote: false,
            parallel_task_limit: 1,
            ..QueueConfig::default()
        });
        assert_eq!(off, "Queued Tasks wait until promotion is turned on, up to 1 at a time");
    }
}
