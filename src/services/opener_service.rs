use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
}

fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

/// Opens `url` in the user's default external browser via the
/// `tauri-plugin-opener` plugin. Webview navigation to an external page is
/// refused by the backend, so an external link can only leave through here.
///
/// Uses the fallible `raw_invoke` binding rather than the `-> JsValue` one
/// most services use: a rejected `open_url` call (no default browser
/// configured, no handler registered for the URL scheme, plugin permission
/// denied, ...) must be reported to the caller, not silently treated as a
/// successful open.
pub async fn open_url_external(url: String) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({ "url": url }))
        .map_err(|e| e.to_string())?;
    JsFuture::from(raw_invoke("plugin:opener|open_url", args))
        .await
        .map_err(js_error_to_string)?;
    Ok(())
}

/// Whether a link destination is an absolute `http`, `https` or `mailto` URL
/// that may be opened externally. Case-insensitive; a destination with
/// whitespace or control characters is refused, because a browser strips
/// those before it reads the scheme.
pub fn is_safe_link_destination(url: &str) -> bool {
    let has_scheme = |scheme: &str| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    };
    ["http://", "https://", "mailto:"]
        .into_iter()
        .any(has_scheme)
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Whether a mouse event opens a link. The primary button opens it through
/// `click` and the middle button through `auxclick`; each button is accepted on
/// its own event only, so one press cannot open a link twice. Any other button
/// keeps its default behavior, so a right-click still reaches the context menu.
fn opens_link(event_type: &str, button: i16) -> bool {
    matches!((event_type, button), ("click", 0) | ("auxclick", 1))
}

/// Routes clicks on external links, anywhere in the document, through
/// `open_url_external`. Installed once at startup. The listener runs in the
/// capture phase, so no component can stop it, and each click is handled once.
pub fn route_external_links() {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let on_mouse = Closure::<dyn Fn(web_sys::MouseEvent)>::new(|ev: web_sys::MouseEvent| {
        if !opens_link(&ev.type_(), ev.button()) {
            return;
        }
        let Some(anchor) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
            .and_then(|el| el.closest("a").ok().flatten())
        else {
            return;
        };
        let Some(href) = anchor.get_attribute("href") else {
            return;
        };
        if !is_safe_link_destination(&href) {
            return;
        }
        ev.prevent_default();
        spawn_local(async move {
            if let Err(e) = open_url_external(href).await {
                crate::components::toast::error(format!("Could not open the link: {e}"));
            }
        });
    });
    let listener: &js_sys::Function = on_mouse.as_ref().unchecked_ref();
    for event in ["click", "auxclick"] {
        let _ = document.add_event_listener_with_callback_and_bool(event, listener, true);
    }
    // The listener lives for the whole session, so the closure is leaked on purpose.
    on_mouse.forget();
}

#[cfg(test)]
mod tests {
    use super::opens_link;

    #[test]
    fn each_button_opens_a_link_only_on_its_own_event() {
        assert!(opens_link("click", 0));
        assert!(opens_link("auxclick", 1));
        assert!(!opens_link("click", 1));
        assert!(!opens_link("auxclick", 0));
    }

    #[test]
    fn the_context_menu_button_never_opens_a_link() {
        assert!(!opens_link("click", 2));
        assert!(!opens_link("auxclick", 2));
        assert!(!opens_link("auxclick", 3));
    }
}
