//! Assertions and bridges against a running SlashIt window.
//!
//! Everything here goes through the same surfaces a user or the frontend
//! itself would use: located elements, real clicks, and `window.__TAURI__`
//! for backend calls. Nothing reaches into the application's memory.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::time::Duration;
use thirtyfour::prelude::*;

use crate::{DEV_URL_PREFIX, ROOT_ELEMENT};

/// How long to wait for an element a journey expects to appear.
pub const ELEMENT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(150);

/// Text WebKit puts on its own failure pages. A development-profile binary
/// with no Trunk server renders exactly one of these, and it has a `<body>`,
/// a title and a non-zero size — so "the window shows something" cannot
/// distinguish it from the application.
const ERROR_PAGE_MARKERS: [&str; 5] = [
    "could not connect",
    "unable to load",
    "err_connection_refused",
    "failed to open page",
    "problem loading",
];

/// Prove the window is really running the packaged SlashIt frontend.
///
/// Three independent things can produce a live Tauri process that is not the
/// application under test: a binary built without `custom-protocol` pointing
/// at `devUrl`, a WebKit error page, and a window whose WASM never mounted.
/// This rules out all three.
pub async fn assert_frontend_is_real(driver: &WebDriver) -> Result<()> {
    let url = driver
        .current_url()
        .await
        .context("could not read the current URL")?
        .to_string();

    if url.starts_with(DEV_URL_PREFIX) {
        bail!(
            "the application is loading the Trunk dev server ({url}). This is a plain \
             `cargo build` binary; acceptance needs `cargo tauri build --debug --no-bundle`, \
             which enables `custom-protocol` and embeds the frontend."
        );
    }
    if !(url.starts_with("tauri://") || url.contains("tauri.localhost")) {
        bail!("the application is not served over Tauri's custom protocol: {url}");
    }

    let source = driver
        .source()
        .await
        .context("could not read the page source")?;
    let lowered = source.to_lowercase();
    if let Some(marker) = ERROR_PAGE_MARKERS
        .iter()
        .find(|marker| lowered.contains(**marker))
    {
        bail!("the window is showing a browser error page (matched {marker:?}), not SlashIt");
    }

    let root = driver
        .query(By::Css(ROOT_ELEMENT))
        .wait(ELEMENT_TIMEOUT, POLL)
        .first()
        .await
        .with_context(|| format!("the Leptos root {ROOT_ELEMENT} is not present"))?;
    if !root
        .is_displayed()
        .await
        .context("could not ask whether the Leptos root is displayed")?
    {
        bail!("{ROOT_ELEMENT} exists but is not displayed — the frontend mounted into a hidden tree");
    }

    Ok(())
}

/// Locate a visible element, or fail saying which selector and how long.
pub async fn visible(driver: &WebDriver, selector: &str) -> Result<WebElement> {
    let element = driver
        .query(By::Css(selector))
        .wait(ELEMENT_TIMEOUT, POLL)
        .first()
        .await
        .with_context(|| {
            format!(
                "{selector} did not appear within {}s",
                ELEMENT_TIMEOUT.as_secs()
            )
        })?;
    if !element
        .is_displayed()
        .await
        .with_context(|| format!("could not ask whether {selector} is displayed"))?
    {
        bail!("{selector} exists but is not displayed");
    }
    Ok(element)
}

/// Assert an element is not in the document right now.
///
/// Used to establish the "before" half of an interaction, so the "after"
/// half cannot be satisfied by something that was already there.
pub async fn assert_absent(driver: &WebDriver, selector: &str) -> Result<()> {
    let found = driver
        .find_all(By::Css(selector))
        .await
        .with_context(|| format!("could not query for {selector}"))?;
    if !found.is_empty() {
        bail!("{selector} was already present before the interaction that should create it");
    }
    Ok(())
}

/// Call a Tauri command through the frontend's own IPC bridge.
///
/// This is the exact path `src/services/` uses, so a success here means the
/// webview, the IPC channel, the command registry and the backend state are
/// all working together — not that a Rust function is callable.
pub async fn invoke(driver: &WebDriver, command: &str, args: Value) -> Result<Value> {
    const SCRIPT: &str = r#"
        const done = arguments[arguments.length - 1];
        const [command, args] = arguments;
        if (!window.__TAURI__ || !window.__TAURI__.core) {
            done({ err: "window.__TAURI__.core is missing; withGlobalTauri is off" });
        } else {
            window.__TAURI__.core.invoke(command, args)
                .then((value) => done({ ok: value === undefined ? null : value }))
                .catch((error) => done({ err: String(error) }));
        }
    "#;

    let returned = driver
        .execute_async(SCRIPT, vec![Value::String(command.to_string()), args])
        .await
        .map_err(|e| anyhow!("invoke({command}) could not be dispatched: {e}"))?;

    let value = returned.json();
    if let Some(error) = value.get("err").and_then(Value::as_str) {
        bail!("invoke({command}) failed in the application: {error}");
    }
    value
        .get("ok")
        .cloned()
        .ok_or_else(|| anyhow!("invoke({command}) returned an unrecognised envelope: {value}"))
}
