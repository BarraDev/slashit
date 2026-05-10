use leptos::prelude::*;
use leptos::web_sys;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use leptos::callback::Callback;

pub fn setup_keyboard_shortcuts(
    set_current_page: WriteSignal<String>,
    on_new_task: Option<Callback<()>>,
    on_refresh: Option<Callback<()>>,
    on_close_modals: Option<Callback<()>>,
) {
    let window = web_sys::window().expect("no global window exists");
    let document = window.document().expect("no document exists");

    let closure = Closure::wrap(Box::new(move |event: web_sys::KeyboardEvent| {
        // Check if the event target is an input element, textarea, or terminal
        // If so, don't trigger global shortcuts
        if let Some(target) = event.target() {
            if let Ok(element) = target.dyn_into::<web_sys::Element>() {
                let tag_name = element.tag_name().to_lowercase();
                
                // Skip shortcuts if typing in input/textarea/select
                if tag_name == "input" || tag_name == "textarea" || tag_name == "select" {
                    return;
                }
                
                // Skip shortcuts if element is contenteditable or has tabindex (like terminal)
                if element.has_attribute("contenteditable") {
                    return;
                }
                
                // Skip if inside a terminal (check for tabindex which terminals use)
                if element.has_attribute("tabindex") {
                    // Check if it's the terminal by looking at classes or parent
                    if let Some(class_list) = element.get_attribute("class") {
                        if class_list.contains("font-mono") || class_list.contains("terminal") {
                            return;
                        }
                    }
                }
            }
        }
        
        let key = event.key();
        let cmd_or_ctrl = event.meta_key() || event.ctrl_key();

        if cmd_or_ctrl {
            match key.as_str() {
                "k" => {
                    event.prevent_default();
                    if let Some(ref cb) = on_new_task {
                        cb.run(());
                    }
                }
                "1" => {
                    event.prevent_default();
                    set_current_page.set("dashboard".to_string());
                }
                "2" => {
                    event.prevent_default();
                    set_current_page.set("agent".to_string());
                }
                "3" => {
                    event.prevent_default();
                    set_current_page.set("roadmap".to_string());
                }
                "4" => {
                    event.prevent_default();
                    set_current_page.set("context".to_string());
                }
                "5" => {
                    event.prevent_default();
                    set_current_page.set("ideation".to_string());
                }
                "n" => {
                    event.prevent_default();
                    set_current_page.set("insights".to_string());
                }
                "r" => {
                    event.prevent_default();
                    if let Some(ref cb) = on_refresh {
                        cb.run(());
                    }
                }
                "," => {
                    event.prevent_default();
                    set_current_page.set("settings".to_string());
                }
                _ => {}
            }
        } else if key.as_str() == "Escape" {
            event.prevent_default();
            if let Some(ref cb) = on_close_modals {
                cb.run(());
            }
        }
    }) as Box<dyn Fn(_)>);

    document
        .add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref())
        .unwrap();

    closure.forget();
}
