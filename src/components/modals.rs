use leptos::prelude::*;
use leptos::callback::Callback;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModalSize {
    Md,
}

/// A confirmation dialog for destructive actions
#[component]
pub fn ConfirmDialog(
    #[prop(into)] show: Signal<bool>,
    set_show: WriteSignal<bool>,
    #[prop(into)] title: String,
    #[prop(into)] message: String,
    #[prop(default = "Delete".to_string())] confirm_text: String,
    #[prop(default = "Cancel".to_string())] cancel_text: String,
    #[prop(default = false)] is_destructive: bool,
    #[prop(into)] on_confirm: Callback<()>,
    #[prop(optional, into)] loading: Option<Signal<bool>>,
) -> impl IntoView 
{
    let is_loading = move || loading.map(|l| l.get()).unwrap_or(false);
    
    let handle_confirm = move |_| {
        on_confirm.run(());
    };

    let handle_cancel = move |_| {
        set_show.set(false);
    };

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4" role="dialog" aria-modal="true">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm animate-fade-in"
                    on:click=handle_cancel
                    aria-hidden="true"
                ></div>

                <div class="relative w-full max-w-md bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl animate-modal-in" role="document">
                    // Header with warning icon
                    <div class="flex items-center gap-4 p-6 border-b border-white/5">
                        <div class=format!(
                            "w-12 h-12 rounded-full flex items-center justify-center {}",
                            if is_destructive { "bg-red-500/20" } else { "bg-yellow-500/20" }
                        )>
                            <svg 
                                class=format!("w-6 h-6 {}", if is_destructive { "text-red-400" } else { "text-yellow-400" })
                                fill="none" 
                                viewBox="0 0 24 24" 
                                stroke="currentColor"
                            >
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 4c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
                            </svg>
                        </div>
                        <div>
                            <h2 class="text-lg font-semibold text-white/90">{title.clone()}</h2>
                        </div>
                    </div>

                    // Message
                    <div class="p-6">
                        <p class="text-white/70">{message.clone()}</p>
                    </div>

                    // Actions
                    <div class="flex items-center justify-end gap-3 p-6 border-t border-white/5">
                        <button
                            on:click=handle_cancel
                            class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                            disabled=is_loading
                            type="button"
                        >
                            {cancel_text.clone()}
                        </button>
                        <button
                            on:click=handle_confirm
                            class=move || format!(
                                "flex items-center gap-2 px-4 py-2 rounded-lg font-medium transition-colors {}",
                                if is_destructive {
                                    if is_loading() {
                                        "bg-red-500/50 text-white/50 cursor-not-allowed"
                                    } else {
                                        "bg-red-500 hover:bg-red-600 text-white"
                                    }
                                } else {
                                    if is_loading() {
                                        "bg-blue-500/50 text-white/50 cursor-not-allowed"
                                    } else {
                                        "bg-blue-500 hover:bg-blue-600 text-white"
                                    }
                                }
                            )
                            disabled=is_loading
                            type="button"
                        >
                            {move || if is_loading() {
                                view! {
                                    <svg class="w-4 h-4 animate-spin" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
                                    </svg>
                                }.into_any()
                            } else {
                                ().into_any()
                            }}
                            {confirm_text.clone()}
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}

#[component]
pub fn Modal(
    #[prop(into)] show: RwSignal<bool>,
    #[prop(into)] title: String,
    #[prop(default = ModalSize::Md)] size: ModalSize,
    #[prop(default = true)] can_close: bool,
    #[prop(default = String::new())] content: String,
) -> impl IntoView {
    let show_close = show;
    let handle_close = move |_| {
        if can_close {
            show_close.set(false);
        }
    };

    let title_memo = Memo::new(move |_| title.clone());
    let content_memo = Memo::new(move |_| content.clone());

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-50 flex items-center justify-center p-4" role="dialog" aria-modal="true">
                <div
                    class="absolute inset-0 bg-black/60 backdrop-blur-sm animate-fade-in"
                    on:click=handle_close
                    aria-hidden="true"
                ></div>

                <div class="relative w-full bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl animate-modal-in" role="document" aria-labelledby="modal-title">
                    <div class="flex items-center justify-between p-6 border-b border-white/5">
                        <h2 id="modal-title" class="text-lg font-semibold text-white/90">{move || title_memo.get()}</h2>
                        {can_close.then(|| view! {
                            <button
                                on:click=handle_close
                                class="p-2 rounded-lg hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                                aria-label="Close modal"
                                type="button"
                            >
                                <svg class="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                                </svg>
                            </button>
                        })}
                    </div>

                    <div class="p-6">
                        <p class="text-white/70">{move || content_memo.get()}</p>
                    </div>
                </div>
            </div>
        </Show>
    }
}
