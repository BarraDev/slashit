use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThemeColors {
    pub background: String,
    pub surface: String,
    pub border: String,
    pub text_primary: String,
    pub text_secondary: String,
    pub accent: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Theme {
    pub id: String,
    pub name: String,
    pub description: String,
    pub colors: ThemeColors,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AppearanceMode {
    System,
    Light,
    Dark,
}
