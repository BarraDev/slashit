use leptos::prelude::*;
use leptos::callback::Callback;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// A single option for the CustomSelect component
#[derive(Clone, Debug, PartialEq)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }
}

/// A custom dropdown select component with full dark theme styling control.
/// Unlike native `<select>` elements, this component allows complete customization
/// of the dropdown appearance including the options list.
#[component]
pub fn CustomSelect(
    /// The list of options to display in the dropdown
    #[prop(into)]
    options: Vec<SelectOption>,
    /// Signal containing the currently selected value
    #[prop(into)]
    selected: Signal<String>,
    /// Callback fired when the selection changes
    #[prop(into)]
    on_change: Callback<String>,
    /// Placeholder text shown when no option is selected
    #[prop(default = "Select...".to_string())]
    placeholder: String,
    /// Whether the select is disabled
    #[prop(default = false)]
    disabled: bool,
) -> impl IntoView {
    let (is_open, set_is_open) = signal(false);
    
    // Store options in a signal for reactive access
    let options_signal = StoredValue::new(options);
    let placeholder_signal = StoredValue::new(placeholder);
    
    // Get the label for the currently selected value
    let selected_label = move || {
        let val = selected.get();
        if val.is_empty() {
            return placeholder_signal.get_value();
        }
        options_signal.get_value()
            .iter()
            .find(|o| o.value == val)
            .map(|o| o.label.clone())
            .unwrap_or_else(|| placeholder_signal.get_value())
    };
    
    // Reference to the dropdown container for click-outside detection
    let container_ref = NodeRef::<leptos::html::Div>::new();
    
    // Click-outside handler to close dropdown
    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true; // Only set up listener once on mount
        }
        
        let closure = Closure::<dyn Fn(web_sys::MouseEvent)>::new(move |ev: web_sys::MouseEvent| {
            // Only process if dropdown is open
            if !is_open.get() {
                return;
            }
            
            if let Some(target) = ev.target() {
                if let Some(element) = target.dyn_ref::<web_sys::Element>() {
                    // Check if click is outside the dropdown container
                    let is_inside = element.closest(".custom-select-container").ok().flatten().is_some();
                    if !is_inside {
                        set_is_open.set(false);
                    }
                }
            }
        });
        
        if let Some(window) = web_sys::window() {
            if let Some(document) = window.document() {
                let _ = document.add_event_listener_with_callback(
                    "click",
                    closure.as_ref().unchecked_ref(),
                );
                // Keep the closure alive for the lifetime of the component
                closure.forget();
            }
        }
        
        true
    });
    
    // Handle option selection
    let handle_select = move |value: String| {
        on_change.run(value);
        set_is_open.set(false);
    };
    
    // Handle keyboard navigation
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        match ev.key().as_str() {
            "Escape" => {
                ev.prevent_default();
                set_is_open.set(false);
            }
            "Enter" | " " => {
                if !is_open.get() {
                    ev.prevent_default();
                    set_is_open.set(true);
                }
            }
            _ => {}
        }
    };

    view! {
        <div 
            node_ref=container_ref
            class="relative custom-select-container"
        >
            // Trigger button
            <button
                type="button"
                disabled=disabled
                on:click=move |ev: web_sys::MouseEvent| {
                    ev.stop_propagation();
                    if !disabled {
                        set_is_open.update(|v| *v = !*v);
                    }
                }
                on:keydown=on_keydown
                class=move || format!(
                    "w-full px-3 py-2 rounded-lg border text-left flex items-center justify-between transition-all duration-150 {}",
                    if disabled {
                        "bg-white/5 border-white/10 text-white/40 cursor-not-allowed"
                    } else if is_open.get() {
                        "bg-white/10 border-blue-500/50 text-white/90 ring-2 ring-blue-500/50"
                    } else {
                        "bg-white/5 border-white/10 text-white/90 hover:border-white/20 hover:bg-white/[0.07]"
                    }
                )
                aria-haspopup="listbox"
                aria-expanded=move || if is_open.get() { "true" } else { "false" }
            >
                <span class=move || {
                    let val = selected.get();
                    if val.is_empty() {
                        "text-white/40"
                    } else {
                        "text-white/90"
                    }
                }>
                    {selected_label}
                </span>
                <svg 
                    class=move || format!(
                        "w-4 h-4 text-white/50 transition-transform duration-150 {}",
                        if is_open.get() { "rotate-180" } else { "" }
                    )
                    fill="none" 
                    viewBox="0 0 24 24" 
                    stroke="currentColor"
                >
                    <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7" />
                </svg>
            </button>
            
            // Dropdown menu
            <Show when=move || is_open.get()>
                <div 
                    class="absolute z-50 w-full mt-1 py-1 rounded-lg bg-[#1a1a1a] border border-white/10 shadow-xl max-h-60 overflow-auto animate-fade-in"
                    role="listbox"
                >
                    {move || {
                        options_signal.get_value().into_iter().map(|opt| {
                            let value = opt.value.clone();
                            let label = opt.label.clone();
                            let is_selected = selected.get() == value;
                            let value_for_click = value.clone();

                            view! {
                                <button
                                    type="button"
                                    role="option"
                                    aria-selected=move || if is_selected { "true" } else { "false" }
                                    on:click=move |ev: web_sys::MouseEvent| {
                                        ev.stop_propagation();
                                        handle_select(value_for_click.clone());
                                    }
                                    class=move || format!(
                                        "w-full px-3 py-2 text-left transition-colors duration-100 {}",
                                        if is_selected {
                                            "bg-blue-500/20 text-blue-300"
                                        } else {
                                            "text-white/80 hover:bg-white/10"
                                        }
                                    )
                                >
                                    <span class="flex items-center gap-2">
                                        {is_selected.then(|| view! {
                                            <svg class="w-4 h-4 text-blue-400" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                                                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M5 13l4 4L19 7" />
                                            </svg>
                                        })}
                                        {label}
                                    </span>
                                </button>
                            }
                        }).collect::<Vec<_>>()
                    }}
                </div>
            </Show>
        </div>
    }
}
