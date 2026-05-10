use leptos::prelude::*;
use std::collections::VecDeque;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ToastVariant {
    Success,
    Error,
    Warning,
    Info,
}

impl ToastVariant {
    fn icon(&self) -> &'static str {
        match self {
            ToastVariant::Success => "✓",
            ToastVariant::Error => "✕",
            ToastVariant::Warning => "⚠",
            ToastVariant::Info => "ⓘ",
        }
    }

    fn class(&self) -> &'static str {
        match self {
            ToastVariant::Success => "border-green-500/50 bg-green-500/10 text-green-300",
            ToastVariant::Error => "border-red-500/50 bg-red-500/10 text-red-300",
            ToastVariant::Warning => "border-yellow-500/50 bg-yellow-500/10 text-yellow-300",
            ToastVariant::Info => "border-blue-500/50 bg-blue-500/10 text-blue-300",
        }
    }
}

#[derive(Clone)]
struct Toast {
    id: uuid::Uuid,
    message: String,
    variant: ToastVariant,
}

thread_local! {
    static TOASTS: RwSignal<VecDeque<Toast>> = RwSignal::new(VecDeque::new());
}

pub fn show_toast(message: String, variant: ToastVariant, duration_ms: Option<u64>) {
    TOASTS.with(|toasts| {
        let id = uuid::Uuid::new_v4();
        let toast = Toast {
            id,
            message,
            variant,
        };

        let mut current = toasts.get();
        current.push_back(toast);

        if current.len() > 5 {
            current.pop_front();
        }

        toasts.set(current);
        
        // Auto-dismiss after duration (default 4 seconds)
        let duration = duration_ms.unwrap_or(4000);
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(duration as u32).await;
            remove_toast(id);
        });
    });
}

fn remove_toast(id: uuid::Uuid) {
    TOASTS.with(|toasts| {
        let mut current = toasts.get();
        current.retain(|t| t.id != id);
        toasts.set(current);
    });
}

#[component]
pub fn ToastContainer() -> impl IntoView {
    let toasts = TOASTS.with(|t| *t);

    view! {
        <div class="fixed bottom-4 right-4 z-50 flex flex-col gap-2 max-w-md">
            {move || {
                toasts.get().into_iter().map(|toast| {
                    let toast_clone = toast.clone();
                    view! {
                        <ToastItem
                            message=toast_clone.message
                            variant=toast_clone.variant
                            id=toast_clone.id
                        />
                    }
                }).collect::<Vec<_>>()
            }}
        </div>
    }
}

#[component]
fn ToastItem(
    #[prop(into)] message: String,
    variant: ToastVariant,
    id: uuid::Uuid,
) -> impl IntoView {
    let handle_close = move |_| {
        remove_toast(id);
    };

    view! {
        <div
            class=format!(
                "flex items-start gap-3 p-4 rounded-lg border shadow-lg animate-toast-in {}",
                variant.class(),
            )
        >
            <div class="flex-shrink-0 w-5 h-5 flex items-center justify-center text-sm font-medium">
                {variant.icon()}
            </div>

            <div class="flex-1 min-w-0">
                <p class="text-sm text-white/90">{message}</p>
            </div>

            <button
                data-testid="toast-dismiss"
                on:click=handle_close
                class="flex-shrink-0 p-1 rounded hover:bg-white/5 text-white/40 hover:text-white/60 transition-colors"
                aria-label="Close notification"
                type="button"
            >
                <svg class="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
                </svg>
            </button>
        </div>
    }
}

pub fn success(message: String) {
    show_toast(message, ToastVariant::Success, None);
}

pub fn error(message: String) {
    show_toast(message, ToastVariant::Error, None);
}

pub fn info(message: String) {
    show_toast(message, ToastVariant::Info, None);
}

