use leptos::prelude::*;
use leptos::task::spawn_local;
use crate::models::QueueConfig;
use crate::services::update_queue_config;

#[component]
pub fn QueueSettingsModal(
    show: RwSignal<bool>,
    project_id: String,
    config: RwSignal<QueueConfig>,
) -> impl IntoView {
    let (parallel_limit, set_parallel_limit) = signal(config.get().parallel_task_limit);
    let (auto_promote, set_auto_promote) = signal(config.get().auto_promote);
    let (fifo_ordering, set_fifo_ordering) = signal(config.get().fifo_ordering);
    let (use_coderabbit, set_use_coderabbit) = signal(config.get().use_coderabbit);
    let (saving, set_saving) = signal(false);
    let (error_msg, set_error_msg) = signal(None::<String>);

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                    on:click=move |_| show.set(false)
                ></div>

                <div class="relative w-full max-w-lg bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl" on:click=move |e| e.stop_propagation()>
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
                                type="number"
                                min="1"
                                max="10"
                                prop:value=move || parallel_limit.get().to_string()
                                on:input=move |ev| {
                                    if let Ok(val) = event_target_value(&ev).parse::<u32>() {
                                        set_parallel_limit.set(val.clamp(1, 10));
                                    }
                                }
                                class="w-full px-3 py-2 rounded-lg bg-white/5 border border-white/10 text-white placeholder-white/30 focus:outline-none focus:ring-2 focus:ring-blue-500/50"
                            />
                            <p class="mt-1 text-xs text-white/40">"Maximum number of tasks that can run simultaneously (1-10)"</p>
                        </div>

                        <label class="flex items-center gap-3 p-4 rounded-xl bg-white/5 border border-white/10 cursor-pointer hover:bg-white/[0.07] transition-colors">
                            <input
                                type="checkbox"
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
                            disabled=move || saving.get()
                        >
                            "Cancel"
                        </button>
                        <button
                            class="px-4 py-2 rounded-lg bg-blue-500 hover:bg-blue-600 text-white font-medium transition-colors disabled:bg-white/5 disabled:text-white/30"
                            on:click=on_save(SaveCtx {
                                project_id: project_id.clone(),
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
    project_id: String,
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
        let project_id = ctx.project_id.clone();
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

            match update_queue_config(project_id, new_config.clone()).await {
                Ok(()) => {
                    config.set(new_config);
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
