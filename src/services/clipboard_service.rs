// SPDX-License-Identifier: MPL-2.0
//! Writing text to the system clipboard, shared by every "copy" control.

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["navigator", "clipboard"], js_name = writeText, catch)]
    fn clipboard_write_text(text: &str) -> Result<js_sys::Promise, JsValue>;
}

/// Puts `text` on the clipboard. The error is a sentence fragment that says
/// why it could not: the clipboard is missing, or it refused the text.
pub async fn write_text(text: &str) -> Result<(), String> {
    match clipboard_write_text(text) {
        Ok(promise) => JsFuture::from(promise)
            .await
            .map(|_| ())
            .map_err(|_| "the clipboard refused the text".to_string()),
        Err(_) => Err("the clipboard is not available".to_string()),
    }
}
