use wasm_bindgen::prelude::*;
use serde::{Deserialize, Serialize};
use leptos::web_sys;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"])]
    async fn invoke(cmd: &str, args: JsValue) -> JsValue;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeColors {
    pub background: String,
    pub surface: String,
    pub border: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub accent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub description: String,
    pub colors: ThemeColors,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AppearanceMode {
    System,
    Light,
    Dark,
}

pub async fn get_theme() -> Result<Theme, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("get_theme", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn set_theme(theme: Theme) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "theme": theme,
    })).unwrap();

    let response = invoke("set_theme", args).await;
    if response.is_undefined() || response.is_null() {
        Ok(())
    } else {
        serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
    }
}

pub async fn set_theme_by_id(theme_id: String) -> Result<(), String> {
    let themes = list_themes().await?;
    let theme = themes
        .into_iter()
        .find(|t| t.id == theme_id)
        .ok_or_else(|| format!("Theme '{}' not found", theme_id))?;
    set_theme(theme).await
}

pub async fn list_themes() -> Result<Vec<Theme>, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("list_themes", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn get_appearance_mode() -> Result<AppearanceMode, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("get_appearance_mode", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn set_appearance_mode(mode: AppearanceMode) -> Result<(), String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({
        "mode": mode,
    })).unwrap();

    let response = invoke("set_appearance_mode", args).await;
    if response.is_undefined() || response.is_null() {
        Ok(())
    } else {
        serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
    }
}

pub async fn get_use_project_rail() -> Result<bool, String> {
    let args = serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap();
    let response = invoke("get_use_project_rail", args).await;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Apply theme colors to CSS custom properties on the document
pub fn apply_theme_to_dom(theme: &Theme) {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            if let Some(root) = document.document_element() {
                let style = root.unchecked_ref::<web_sys::HtmlElement>().style();
                let _ = style.set_property("--theme-background", &theme.colors.background);
                let _ = style.set_property("--theme-surface", &theme.colors.surface);
                let _ = style.set_property("--theme-border", &theme.colors.border);
                let _ = style.set_property("--theme-text-primary", &theme.colors.text_primary);
                let _ = style.set_property("--theme-text-secondary", &theme.colors.text_secondary);
                let _ = style.set_property("--theme-accent", &theme.colors.accent);
                let _ = root.set_attribute("data-theme", &theme.id);
            }
        }
    }
}
