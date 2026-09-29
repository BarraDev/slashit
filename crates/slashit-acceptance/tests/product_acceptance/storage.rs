//! Settings > Storage shows how much disk SlashIt uses, and only shows it.
//!
//! The journey measures, gives SlashIt's own log directory a file of known
//! size, refreshes, and reads the change back from the screen. The host's
//! real disk size decides the pressure level, so the journey only checks
//! that one of the three levels is shown, never which.

use super::human_review::{attribute, click};
use super::*;

const SETTINGS_STORAGE_TAB: &str = "[data-testid=\"settings-tab-storage\"]";
const DISK_USAGE: &str = "[data-testid=\"disk-usage\"]";
const FREE: &str = "[data-testid=\"disk-usage-free\"]";
const PRESSURE: &str = "[data-testid=\"disk-usage-pressure\"]";
const OWNED: &str = "[data-testid=\"disk-usage-owned\"]";
const CONSUMERS: &str = "[data-testid=\"disk-usage-consumers\"]";
const MEASURED_AT: &str = "[data-testid=\"disk-usage-measured-at\"]";
const REFRESH: &str = "[data-testid=\"disk-usage-refresh\"]";

/// Large enough that no other change in a fresh state root can hide it.
const LOG_BYTES: usize = 3 * 1024 * 1024;

#[tokio::test(flavor = "multi_thread")]
async fn storage_settings_measure_disk_usage_and_offer_nothing_destructive() {
    let context = TestContext::new("storage_usage").expect("harness setup");
    let outcome = storage_journey(&context).await;
    context.finish(outcome);
}

async fn storage_journey(context: &TestContext) -> Result<()> {
    let session = context.start_session("storage").await?;
    let outcome = measure_and_refresh(session.driver(), context).await;
    context.close_session(session, "storage", &outcome).await?;
    outcome
}

async fn measure_and_refresh(driver: &WebDriver, context: &TestContext) -> Result<()> {
    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); return true;",
        vec![],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;
    click(driver, SETTINGS_STORAGE_TAB, "the Storage settings").await?;

    // Opening the section measures once by itself.
    await_text(driver, MEASURED_AT, |t| t.starts_with("Measured "), "the first measurement").await?;
    let free = await_text(driver, FREE, |t| !t.is_empty(), "free space").await?;
    if !free.contains(" free of ") {
        bail!("free space reads {free:?}");
    }
    let pressure = attribute(driver, PRESSURE, "data-pressure").await?;
    if !matches!(pressure.as_deref(), Some("normal" | "warning" | "critical")) {
        bail!("the pressure level is {pressure:?}");
    }
    let owned_before = await_text(driver, OWNED, |t| !t.is_empty(), "SlashIt's total").await?;

    // Nothing here may delete, clean or prune: the only control is Refresh.
    let controls = page(
        driver,
        "return Array.from(document.querySelectorAll(arguments[0] + ' button, ' + arguments[0] + ' a'))\
             .map(e => e.textContent.trim());",
        vec![json!(DISK_USAGE)],
    )
    .await?;
    if controls != json!(["Refresh"]) {
        bail!("the disk usage section offers {controls}");
    }

    let log = context.state().data_home().join("slashit-app").join("logs").join("journey.log");
    std::fs::create_dir_all(log.parent().context("the log has no directory")?)?;
    std::fs::write(&log, pseudo_random(LOG_BYTES))?;

    click(driver, REFRESH, "Refresh").await?;
    let owned_after = await_text(
        driver,
        OWNED,
        |t| t != owned_before && (t.ends_with(" MiB") || t.ends_with(" GiB")),
        "SlashIt's total after the log was written",
    )
    .await?;
    let listed = await_text(driver, CONSUMERS, |t| t.contains("Logs"), "the log directory listed").await?;
    if !listed.contains("App data") {
        bail!("the logs are not listed as SlashIt's own data: {listed:?}");
    }

    // The backend agrees with the screen, and measuring removed nothing.
    let status = ui::invoke(driver, "get_storage_usage", json!({})).await?;
    let owned = status["summary"]["slashit_owned_bytes"].as_u64().unwrap_or_default();
    if owned < LOG_BYTES as u64 {
        bail!("the backend counts {owned} bytes, less than the {LOG_BYTES}-byte log ({owned_after} on screen)");
    }
    if status["measuring"] != json!(false) {
        bail!("a measurement is still running after Refresh finished: {status}");
    }
    if std::fs::metadata(&log)?.len() != LOG_BYTES as u64 {
        bail!("measuring changed the log file");
    }
    Ok(())
}

/// Content no filesystem can compress below its length.
fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}
