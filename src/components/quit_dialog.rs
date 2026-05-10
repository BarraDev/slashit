use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::closure::Closure;
use crate::services::force_quit;

#[component]
pub fn QuitDialog() -> impl IntoView {
    let (show, set_show) = signal(false);
    let (pty_count, set_pty_count) = signal(0usize);
    let (agent_count, set_agent_count) = signal(0usize);

    // Listen for quit-requested event from Tauri backend
    Effect::new(move || {
        spawn_local(async move {
            let window = web_sys::window().unwrap();
            let tauri = js_sys::Reflect::get(&window, &JsValue::from_str("__TAURI__")).unwrap();
            let event_mod = js_sys::Reflect::get(&tauri, &JsValue::from_str("event")).unwrap();
            let listen_fn = js_sys::Reflect::get(&event_mod, &JsValue::from_str("listen")).unwrap();
            let listen_fn: js_sys::Function = listen_fn.into();

            let callback = Closure::wrap(Box::new(move |event: JsValue| {
                if let Ok(payload) = js_sys::Reflect::get(&event, &JsValue::from_str("payload")) {
                    if let Ok(pty) = js_sys::Reflect::get(&payload, &JsValue::from_str("pty_count")) {
                        set_pty_count.set(pty.as_f64().unwrap_or(0.0) as usize);
                    }
                    if let Ok(agents) = js_sys::Reflect::get(&payload, &JsValue::from_str("agent_count")) {
                        set_agent_count.set(agents.as_f64().unwrap_or(0.0) as usize);
                    }
                }
                set_show.set(true);
            }) as Box<dyn FnMut(JsValue)>);

            let _ = listen_fn.call2(
                &JsValue::NULL,
                &JsValue::from_str("quit-requested"),
                callback.as_ref(),
            );
            callback.forget(); // Leak the closure to keep it alive
        });
    });

    view! {
        <Show when=move || show.get()>
            <div class="fixed inset-0 z-[100] flex items-center justify-center p-4">
                <div class="absolute inset-0 bg-black/60 backdrop-blur-sm" on:click=move |_| set_show.set(false)></div>
                <div class="relative w-full max-w-md bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl" on:click=move |e| e.stop_propagation()>
                    <div class="p-6">
                        <div class="flex items-center gap-3 mb-4">
                            <svg class="w-6 h-6 text-amber-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2"
                                    d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-2.5L13.732 4c-.77-.833-1.964-.833-2.732 0L4.082 16.5c-.77.833.192 2.5 1.732 2.5z" />
                            </svg>
                            <h2 class="text-lg font-semibold text-white/90">"Active Processes"</h2>
                        </div>
                        <p class="text-white/60 text-sm mb-4">
                            "There are still active processes running:"
                        </p>
                        <div class="space-y-2 mb-6">
                            <Show when=move || pty_count.get() != 0>
                                <div class="flex items-center gap-2 text-sm text-white/70">
                                    <span class="w-2 h-2 rounded-full bg-green-400"></span>
                                    {move || format!("{} terminal session{}", pty_count.get(), if pty_count.get() != 1 { "s" } else { "" })}
                                </div>
                            </Show>
                            <Show when=move || agent_count.get() != 0>
                                <div class="flex items-center gap-2 text-sm text-white/70">
                                    <span class="w-2 h-2 rounded-full bg-blue-400"></span>
                                    {move || format!("{} running agent{}", agent_count.get(), if agent_count.get() != 1 { "s" } else { "" })}
                                </div>
                            </Show>
                        </div>
                        <p class="text-white/40 text-xs mb-6">
                            "Quitting will terminate all running processes."
                        </p>
                    </div>
                    <div class="flex items-center justify-end gap-3 p-6 border-t border-white/5">
                        <button
                            on:click=move |_| set_show.set(false)
                            class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                        >
                            "Cancel"
                        </button>
                        <button
                            on:click=move |_| {
                                spawn_local(async move {
                                    let _ = force_quit().await;
                                });
                            }
                            class="px-4 py-2 rounded-lg bg-red-500 hover:bg-red-600 text-white font-medium transition-colors"
                        >
                            "Quit Anyway"
                        </button>
                    </div>
                </div>
            </div>
        </Show>
    }
}
