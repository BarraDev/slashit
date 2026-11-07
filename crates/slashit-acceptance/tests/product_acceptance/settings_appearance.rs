// SPDX-License-Identifier: MPL-2.0
//! Settings marks the section the Human is in, and its Theme section offers no
//! appearance choice it cannot keep.
//!
//! Switching sections moves the selected marker, both the visual class and the
//! accessible `aria-current` attribute. The Theme section shows the dark
//! appearance as static text, with no button for Light, System or a Color
//! Theme. Nothing here uses the WebView inspector or an IPC command: the only
//! way in is the page.

use super::human_review::click;
use super::*;

const SELECTED_CLASS: &str = "bg-blue-500/20";

#[tokio::test(flavor = "multi_thread")]
async fn settings_marks_the_open_section_and_offers_only_dark_appearance() {
    let context = TestContext::new("settings_appearance").expect("harness setup");
    let outcome = settings_appearance_journey(&context).await;
    context.finish(outcome);
}

async fn settings_appearance_journey(context: &TestContext) -> Result<()> {
    let session = context.start_session("settings-appearance").await?;
    let outcome = check_sections(session.driver()).await;
    context
        .close_session(session, "settings-appearance", &outcome)
        .await?;
    outcome
}

async fn check_sections(driver: &WebDriver) -> Result<()> {
    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); return true;",
        vec![],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;

    await_selected_tab(driver, "general").await?;

    click(driver, &tab_selector("queue"), "the Queue settings tab").await?;
    await_selected_tab(driver, "queue").await?;

    click(driver, &tab_selector("theme"), "the Theme settings tab").await?;
    await_selected_tab(driver, "theme").await?;
    appearance_offers_only_dark(driver).await?;

    click(driver, &tab_selector("general"), "the General settings tab").await?;
    await_selected_tab(driver, "general").await
}

fn tab_selector(tab: &str) -> String {
    format!("[data-testid=\"settings-tab-{tab}\"]")
}

/// Waits until exactly the named tab carries both the selected class and `aria-current`.
async fn await_selected_tab(driver: &WebDriver, tab: &str) -> Result<()> {
    let wanted = format!("settings-tab-{tab}");
    let started = Instant::now();
    loop {
        let tabs = settings_tabs(driver).await?;
        let agrees = !tabs.is_empty()
            && tabs.iter().all(|entry| {
                let is_wanted = entry["id"].as_str() == Some(wanted.as_str());
                (entry["current"].as_str() == Some("true")) == is_wanted
                    && entry["selected"].as_bool() == Some(is_wanted)
            });
        if agrees {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(10) {
            bail!("after choosing {tab}, the Settings tabs show {tabs:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn settings_tabs(driver: &WebDriver) -> Result<Vec<Value>> {
    let script = format!(
        "return Array.from(document.querySelectorAll('[data-testid^=\"settings-tab-\"]'), t => ({{ \
            id: t.dataset.testid, \
            current: t.getAttribute('aria-current'), \
            selected: t.classList.contains('{SELECTED_CLASS}') \
        }}));"
    );
    let value = page(driver, &script, vec![]).await?;
    value
        .as_array()
        .cloned()
        .context("the Settings tabs did not render")
}

/// The Theme section must show Dark as text and expose no actionable control.
async fn appearance_offers_only_dark(driver: &WebDriver) -> Result<()> {
    let view = page(
        driver,
        "const heading = Array.from(document.querySelectorAll('h2')).find(h => h.textContent === 'Appearance');\
         const scope = heading ? heading.closest('.space-y-6') : null;\
         return scope ? { text: scope.innerText, buttons: scope.querySelectorAll('button').length } : null;",
        vec![],
    )
    .await?;
    let Some(text) = view["text"].as_str() else {
        bail!("the Theme section did not render its Appearance heading");
    };
    if !text.contains("Dark") {
        bail!("the Theme section does not state the dark appearance: {text:?}");
    }
    // The explanation may mention Light or system modes; the controls must be gone.
    if text.contains("Color Theme") {
        bail!("the Theme section still presents a Color Theme picker: {text:?}");
    }
    if view["buttons"].as_u64() != Some(0) {
        bail!(
            "the Theme section still exposes {} button(s)",
            view["buttons"]
        );
    }
    Ok(())
}
