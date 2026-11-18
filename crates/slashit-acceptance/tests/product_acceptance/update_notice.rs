// SPDX-License-Identifier: MPL-2.0
//! The available-update notice fits a narrow and a wide window and keeps its
//! actions readable, and it shows release times in local form with the exact
//! value on hover. Settings still carries the full detail, and Later hides the
//! offer from both.
//!
//! The release feed is a loopback fixture, so nothing reaches GitHub. The
//! application reads its listing and download locations from the environment
//! because the acceptance build enables `updater-test-endpoints`.

use super::human_review::click;
use super::*;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Both widths are checked: the notice must fit each one. The main window's
/// minimum width is 900 px, so 900 is the narrowest layout a person can reach.
const WIDE_WINDOW: (u32, u32) = (1280, 800);
const NARROW_WINDOW: (u32, u32) = (900, 760);
const RELEASE: &str = "2026-11-17T10:00:00Z";
const AVAILABLE: &str = "SlashIt 0.0.5 is available";
/// One line of text-xs with padding. A taller button has wrapped.
const MAX_BUTTON_HEIGHT: f64 = 40.0;

#[tokio::test(flavor = "multi_thread")]
async fn update_notice_fits_both_widths_and_settings_keeps_the_detail() {
    let context = TestContext::new("update_notice").expect("harness setup");
    let outcome = update_notice_journey(&context).await;
    context.finish(outcome);
}

async fn update_notice_journey(context: &TestContext) -> Result<()> {
    let feed = Feed::start().await?;
    context.set_child_env("SLASHIT_FEATURE_AUTO_UPDATE", "1");
    context.set_child_env("SLASHIT_UPDATER_TEST_RELEASES_API", feed.listing());
    context.set_child_env("SLASHIT_UPDATER_TEST_DOWNLOAD_BASE", feed.download_base());
    let session = context.start_session("update-notice").await?;
    let outcome = check_notice(session.driver()).await;
    context
        .close_session(session, "update-notice", &outcome)
        .await?;
    outcome
}

async fn check_notice(driver: &WebDriver) -> Result<()> {
    page(
        driver,
        "localStorage.setItem('slashit_current_page', 'settings'); return true;",
        vec![],
    )
    .await?;
    driver.refresh().await?;
    ui::assert_frontend_is_real(driver).await?;

    click(
        driver,
        "[data-testid=\"settings-tab-updates\"]",
        "the Updates settings tab",
    )
    .await?;
    await_selected_updates_tab(driver).await?;
    press_check(driver).await?;
    await_notice(
        driver,
        |notice| notice.is_some(),
        "the available-update notice",
    )
    .await?;

    for (name, (width, height)) in [("wide", WIDE_WINDOW), ("narrow", NARROW_WINDOW)] {
        driver
            .set_window_rect(0, 0, width, height)
            .await
            .context("could not size the window")?;
        let notice = await_notice(driver, |notice| notice.is_some_and(fits), name).await?;
        check_actions(&notice, name)?;
        check_release_stamp(&notice, RELEASE, name)?;
        screenshot(driver, &format!("update-notice-{name}.png")).await?;
    }

    check_settings_detail(driver).await?;

    // Later hides the offer from the notice and from Settings alike; the
    // persistent form is Skip, and neither is changed by this release.
    press_notice_button(driver, "Later").await?;
    await_notice(driver, |notice| notice.is_none(), "the notice after Later").await?;
    if settings_offers_update(driver).await? {
        bail!("Later should hide the offer from Settings for this session, as it does from the notice");
    }
    Ok(())
}

/// The floating notice as the page reports it, read in one script.
struct Notice(Value);

impl Notice {
    fn text(&self) -> &str {
        self.0["text"].as_str().unwrap_or_default()
    }
}

fn fits(notice: &Notice) -> bool {
    let card = &notice.0["card"];
    let (w, h) = (
        notice.0["viewport"]["w"].as_f64().unwrap_or(0.0),
        notice.0["viewport"]["h"].as_f64().unwrap_or(0.0),
    );
    card["l"].as_f64().unwrap_or(-1.0) >= 0.0
        && card["t"].as_f64().unwrap_or(-1.0) >= 0.0
        && card["r"].as_f64().unwrap_or(f64::MAX) <= w
        && card["b"].as_f64().unwrap_or(f64::MAX) <= h
}

fn check_actions(notice: &Notice, width: &str) -> Result<()> {
    let buttons = notice.0["buttons"].as_array().cloned().unwrap_or_default();
    let labels: Vec<&str> = buttons.iter().filter_map(|b| b["label"].as_str()).collect();
    for wanted in ["Skip this version", "Later"] {
        if !labels.contains(&wanted) {
            bail!("{width}: the notice has no \"{wanted}\" button; it shows {labels:?}");
        }
    }
    if labels.contains(&"Download and install") {
        bail!("{width}: an unsupported build offered an install button: {labels:?}");
    }
    let card = &notice.0["card"];
    for button in &buttons {
        let label = button["label"].as_str().unwrap_or("?");
        let rect = &button["rect"];
        let height = rect["h"].as_f64().unwrap_or(f64::MAX);
        if height > MAX_BUTTON_HEIGHT {
            bail!("{width}: \"{label}\" is {height}px tall, so its text wrapped");
        }
        if button["wrap"].as_str() != Some("nowrap") {
            bail!("{width}: \"{label}\" may wrap its text: {}", button["wrap"]);
        }
        let inside = rect["l"].as_f64() >= card["l"].as_f64()
            && rect["r"].as_f64() <= card["r"].as_f64()
            && rect["b"].as_f64() <= card["b"].as_f64();
        if !inside {
            bail!("{width}: \"{label}\" spills out of the notice: {rect} in {card}");
        }
    }
    if !notice.text().contains("See Settings > Updates.") {
        bail!(
            "{width}: the notice does not point to Settings > Updates: {:?}",
            notice.text()
        );
    }
    Ok(())
}

fn check_release_stamp(notice: &Notice, raw: &str, width: &str) -> Result<()> {
    let released = &notice.0["released"];
    let shown = released["text"].as_str().unwrap_or_default();
    if !shown.starts_with("Released ") {
        bail!("{width}: the release line is missing: {released}");
    }
    if released["title"].as_str() != Some(raw) {
        bail!(
            "{width}: the release tooltip is {}, expected {raw:?}",
            released["title"]
        );
    }
    if raw == RELEASE && shown == format!("Released {RELEASE}") {
        bail!("{width}: the release date was not localized: {shown:?}");
    }
    Ok(())
}

async fn check_settings_detail(driver: &WebDriver) -> Result<()> {
    let view = page(
        driver,
        "const body = document.querySelector('.prose');\
         const heading = Array.from(document.querySelectorAll('.prose h2')).map(h => h.textContent);\
         return { notes: body ? body.innerText : null, headings: heading };",
        vec![],
    )
    .await?;
    let notes = view["notes"].as_str().unwrap_or_default();
    if !notes.contains("Scratch test release for layout checks.") {
        bail!("Settings does not show the release notes: {view}");
    }
    if !view["headings"]
        .as_array()
        .is_some_and(|h| h.iter().any(|t| t == "Changes"))
    {
        bail!("the release notes were not rendered as Markdown: {view}");
    }
    Ok(())
}

async fn await_selected_updates_tab(driver: &WebDriver) -> Result<()> {
    await_condition("the Updates tab is selected", || async {
        let value = page(
            driver,
            "const t = document.querySelector('[data-testid=\"settings-tab-updates\"]'); return t ? t.getAttribute('aria-current') : null;",
            vec![],
        )
        .await?;
        Ok(value.as_str() == Some("true"))
    })
    .await
}

/// Presses the Settings check button, which is the path a person uses.
async fn press_check(driver: &WebDriver) -> Result<()> {
    let pressed = page(
        driver,
        "const b = Array.from(document.querySelectorAll('button')).find(b => b.textContent.trim() === 'Check for updates'); if (!b || b.disabled) return false; b.click(); return true;",
        vec![],
    )
    .await?;
    if pressed != json!(true) {
        bail!("the Check for updates button was not available to press");
    }
    Ok(())
}

async fn press_notice_button(driver: &WebDriver, label: &str) -> Result<()> {
    let script = format!(
        "const box = document.querySelector('.fixed.top-4.right-4'); \
         const b = box ? Array.from(box.querySelectorAll('button')).find(b => b.textContent.trim() === '{label}') : null; \
         if (!b) return false; b.click(); return true;"
    );
    if page(driver, &script, vec![]).await? != json!(true) {
        bail!("the notice has no \"{label}\" button to press");
    }
    Ok(())
}

async fn settings_offers_update(driver: &WebDriver) -> Result<bool> {
    let value = page(
        driver,
        "return Array.from(document.querySelectorAll('h2, p')).some(e => e.textContent === 'SlashIt 0.0.5 is available');",
        vec![],
    )
    .await?;
    Ok(value == json!(true))
}

/// Reads the floating notice, or `None` when it is not shown.
async fn read_notice(driver: &WebDriver) -> Result<Option<Notice>> {
    let value = page(
        driver,
        r#"
        const box = document.querySelector('.fixed.top-4.right-4');
        const card = box && Array.from(box.querySelectorAll('p'))
            .find(p => p.textContent === arguments[0])?.closest('div.rounded-xl');
        if (!card) return null;
        const rect = e => { const b = e.getBoundingClientRect(); return { l: b.left, t: b.top, r: b.right, b: b.bottom, h: b.height }; };
        const released = Array.from(card.querySelectorAll('p')).find(p => p.textContent.startsWith('Released '));
        return {
            viewport: { w: window.innerWidth, h: window.innerHeight },
            card: rect(card),
            text: card.innerText,
            released: released ? { text: released.textContent, title: released.getAttribute('title') } : null,
            buttons: Array.from(card.querySelectorAll('button')).map(b => ({
                label: b.textContent.trim(),
                rect: rect(b),
                wrap: getComputedStyle(b).whiteSpace,
            })),
        };
        "#,
        vec![json!(AVAILABLE)],
    )
    .await?;
    Ok((!value.is_null()).then(|| Notice(value)))
}

/// Waits until `ready` accepts what the notice shows, or fails with the last state seen.
async fn await_notice(
    driver: &WebDriver,
    ready: impl Fn(Option<&Notice>) -> bool,
    what: &str,
) -> Result<Notice> {
    let started = Instant::now();
    loop {
        let notice = read_notice(driver).await?;
        if ready(notice.as_ref()) {
            if let Some(notice) = notice {
                return Ok(notice);
            }
            return Ok(Notice(Value::Null));
        }
        if started.elapsed() > Duration::from_secs(15) {
            let last = notice
                .map(|n| n.0.to_string())
                .unwrap_or_else(|| "absent".to_string());
            bail!("waiting for {what}: the notice was {last}");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn await_condition<F, Fut>(what: &str, check: F) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let started = Instant::now();
    loop {
        if check().await? {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(10) {
            bail!("timed out waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn screenshot(driver: &WebDriver, name: &str) -> Result<()> {
    let Some(dir) = std::env::var_os("SLASHIT_ACCEPTANCE_SCREENSHOTS") else {
        return Ok(());
    };
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    driver
        .screenshot(&dir.join(name))
        .await
        .with_context(|| format!("could not save {name}"))
}

/// A loopback release feed that always offers `v0.0.5`, released at `RELEASE`.
struct Feed {
    address: SocketAddr,
}

impl Feed {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("could not open the release feed fixture")?;
        let address = listener.local_addr()?;
        // The manifest names its payload, and the updater only accepts a payload under the download base.
        let download_base = format!("http://{address}/download");
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let download_base = download_base.clone();
                tokio::spawn(async move {
                    let _ = answer(socket, &download_base).await;
                });
            }
        });
        Ok(Self { address })
    }

    fn listing(&self) -> String {
        format!("http://{}/repos/x/y/releases", self.address)
    }

    fn download_base(&self) -> String {
        format!("http://{}/download", self.address)
    }
}

async fn answer(mut socket: TcpStream, download_base: &str) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 4096];
    let read = socket.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let path = request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    let (status, body) = match path.as_str() {
        "/repos/x/y/releases" => (
            "200 OK",
            json!([{ "tag_name": "v0.0.5", "draft": false, "prerelease": false, "assets": [{ "name": "latest.json" }] }])
                .to_string(),
        ),
        "/download/v0.0.5/latest.json" => {
            (
                "200 OK",
                json!({
                    "version": "0.0.5",
                    "notes": "## Changes\n\n- Scratch test release for layout checks.",
                    "pub_date": RELEASE,
                    "platforms": { "linux-x86_64": { "signature": "ZHVtbXk=", "url": format!("{download_base}/SlashIt_0.0.5.AppImage") } }
                })
                .to_string(),
            )
        }
        _ => ("404 Not Found", String::new()),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}
