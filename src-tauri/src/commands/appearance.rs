use crate::domain::appearance::{Theme, AppearanceMode, ThemeColors};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppearanceState {
    pub current_theme: Arc<Mutex<String>>,
    pub appearance_mode: Arc<Mutex<AppearanceMode>>,
    pub use_project_rail: Arc<Mutex<bool>>,
}

impl AppearanceState {
    pub fn new() -> Self {
        Self {
            current_theme: Arc::new(Mutex::new("default".to_string())),
            appearance_mode: Arc::new(Mutex::new(AppearanceMode::Dark)),
            use_project_rail: Arc::new(Mutex::new(true)),
        }
    }
}

impl Default for AppearanceState {
    fn default() -> Self {
        Self::new()
    }
}

#[tauri::command]
pub async fn get_theme(state: tauri::State<'_, crate::AppState>) -> Result<Theme, String> {
    let themes = list_themes_internal();
    let current_theme_id = state.appearance.current_theme.lock().unwrap().clone();
    Ok(themes
        .into_iter()
        .find(|t| t.id == current_theme_id)
        .unwrap_or_else(default_theme))
}

#[tauri::command]
pub async fn set_theme(
    state: tauri::State<'_, crate::AppState>,
    theme: Theme,
) -> Result<(), String> {
    if let Ok(mut current) = state.appearance.current_theme.lock() {
        *current = theme.id.clone();
    }
    Ok(())
}

#[tauri::command]
pub async fn list_themes() -> Vec<Theme> {
    list_themes_internal()
}

fn list_themes_internal() -> Vec<Theme> {
    vec![
        Theme {
            id: "default".to_string(),
            name: "Default".to_string(),
            description: "Oscura-inspired with pale yellow accent".to_string(),
            colors: ThemeColors {
                background: "#1E1E1E".to_string(),
                surface: "#2A2A2A".to_string(),
                border: "#3A3A3A".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#A0A0A0".to_string(),
                accent: "#FACC15".to_string(),
            },
        },
        Theme {
            id: "dusk".to_string(),
            name: "Dusk".to_string(),
            description: "Warmer variant with orange accents".to_string(),
            colors: ThemeColors {
                background: "#1E1A1A".to_string(),
                surface: "#2A2525".to_string(),
                border: "#3A3030".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#A09090".to_string(),
                accent: "#FB923C".to_string(),
            },
        },
        Theme {
            id: "lime".to_string(),
            name: "Lime".to_string(),
            description: "Fresh, energetic lime with purple".to_string(),
            colors: ThemeColors {
                background: "#1E1E1E".to_string(),
                surface: "#2A2A2A".to_string(),
                border: "#3A3A3A".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#A0A0A0".to_string(),
                accent: "#A3E635".to_string(),
            },
        },
        Theme {
            id: "ocean".to_string(),
            name: "Ocean".to_string(),
            description: "Calm blue tones".to_string(),
            colors: ThemeColors {
                background: "#1A1E2E".to_string(),
                surface: "#252A3A".to_string(),
                border: "#353A4A".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#9090A0".to_string(),
                accent: "#3B82F6".to_string(),
            },
        },
        Theme {
            id: "retro".to_string(),
            name: "Retro".to_string(),
            description: "Warm amber vibes".to_string(),
            colors: ThemeColors {
                background: "#1E1A15".to_string(),
                surface: "#2A2520".to_string(),
                border: "#3A3025".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#A09085".to_string(),
                accent: "#F59E0B".to_string(),
            },
        },
        Theme {
            id: "neo".to_string(),
            name: "Neo".to_string(),
            description: "Cyberpunk pink/magenta".to_string(),
            colors: ThemeColors {
                background: "#1E151E".to_string(),
                surface: "#2A202A".to_string(),
                border: "#3A253A".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#A080A0".to_string(),
                accent: "#EC4899".to_string(),
            },
        },
        Theme {
            id: "forest".to_string(),
            name: "Forest".to_string(),
            description: "Natural green tones".to_string(),
            colors: ThemeColors {
                background: "#151E1A".to_string(),
                surface: "#202A25".to_string(),
                border: "#253A30".to_string(),
                text_primary: "#FFFFFF".to_string(),
                text_secondary: "#80A090".to_string(),
                accent: "#10B981".to_string(),
            },
        },
    ]
}

fn default_theme() -> Theme {
    Theme {
        id: "default".to_string(),
        name: "Default".to_string(),
        description: "Oscura-inspired with pale yellow accent".to_string(),
        colors: ThemeColors {
            background: "#1E1E1E".to_string(),
            surface: "#2A2A2A".to_string(),
            border: "#3A3A3A".to_string(),
            text_primary: "#FFFFFF".to_string(),
            text_secondary: "#A0A0A0".to_string(),
            accent: "#FACC15".to_string(),
        },
    }
}

#[tauri::command]
pub async fn get_appearance_mode(state: tauri::State<'_, crate::AppState>) -> Result<AppearanceMode, String> {
    Ok(state.appearance.appearance_mode.lock().unwrap().clone())
}

#[tauri::command]
pub async fn set_appearance_mode(
    state: tauri::State<'_, crate::AppState>,
    mode: AppearanceMode,
) -> Result<(), String> {
    if let Ok(mut current_mode) = state.appearance.appearance_mode.lock() {
        *current_mode = mode;
    }
    Ok(())
}

#[tauri::command]
pub async fn get_use_project_rail(state: tauri::State<'_, crate::AppState>) -> Result<bool, String> {
    Ok(*state.appearance.use_project_rail.lock().unwrap())
}

#[tauri::command]
pub async fn set_use_project_rail(
    state: tauri::State<'_, crate::AppState>,
    enabled: bool,
) -> Result<(), String> {
    if let Ok(mut val) = state.appearance.use_project_rail.lock() {
        *val = enabled;
    }
    Ok(())
}
