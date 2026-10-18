//! The main window stays on the application's own origin.
//!
//! A page in the main window must not be able to take the window somewhere
//! else. The journey asks the window to leave twice, once with a top-level
//! navigation and once with a request for a new window, and checks that both
//! are refused: the window keeps its address and its window count, and the
//! external fixture, which records every request it receives, records none.
//! The window is then used again to prove that refusing did not break it.
//!
//! The fixture stands in for an external site. It listens on 127.0.0.1 only
//! and serves one fixed page, so the journey contacts nothing on the Internet.
//! It does not click an external link: a link is routed to the system browser
//! by design, so clicking one here would open a real browser.
//!
//! What each half proves. The navigation half is the regression check: on a
//! build without the boundary, the script's navigation reaches the fixture.
//! The new-window half is weaker on Linux. WebKitGTK creates no window unless
//! the application installs a new-window handler, so this half would also pass
//! without the boundary. It still fails if a future change allows new windows,
//! and it checks that a refused request leaves no window and no request.

use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const RAIL_ADD_PROJECT: &str = "[data-testid=\"rail-add-project\"]";
const WIZARD: &str = "[data-testid=\"create-project-modal\"]";
const POPUP_BUTTON: &str = "[data-testid=\"journey-popup-button\"]";

/// Long enough for a refused navigation or window to reach the fixture if the
/// boundary were missing. Both are requested by the application, not timed.
const SETTLE: Duration = Duration::from_secs(2);

/// A real button, so the popup request comes from a user gesture as it would
/// in the product, not from a script call the window could block for another
/// reason.
const POPUP_SCRIPT: &str = "\
    const url = arguments[0];\
    const button = document.createElement('button');\
    button.setAttribute('data-testid', 'journey-popup-button');\
    button.textContent = 'Open popup';\
    button.style.cssText = 'position:fixed;left:0;top:0;width:120px;height:40px;z-index:2147483647';\
    button.addEventListener('click', () => { window.open(url, '_blank'); });\
    document.body.appendChild(button);\
    return true;";

#[tokio::test(flavor = "multi_thread")]
async fn main_window_refuses_external_navigation_and_new_windows() {
    let context = TestContext::new("main_window_navigation").expect("harness setup");
    let outcome = main_window_journey(&context).await;
    context.finish(outcome);
}

async fn main_window_journey(context: &TestContext) -> Result<()> {
    let fixture = Fixture::start().await?;
    let session = context.start_session("main_window").await?;
    let outcome = refusals(session.driver(), &fixture).await;
    context.close_session(session, "main_window", &outcome).await?;
    outcome
}

async fn refusals(driver: &WebDriver, fixture: &Fixture) -> Result<()> {
    ui::assert_frontend_is_real(driver).await?;
    let home = driver.current_url().await?.to_string();
    // WebKitGTK reports the bare origin, without a trailing slash.
    if home != "tauri://localhost" && !home.starts_with("tauri://localhost/") {
        bail!("the main window loaded {home}, not the application's own origin");
    }
    let windows = driver.windows().await?.len();

    // A top-level navigation, started the way a script in the page would start it.
    page(
        driver,
        "window.location.assign(arguments[0]); return true;",
        vec![json!(fixture.url("/navigate"))],
    )
    .await?;
    tokio::time::sleep(SETTLE).await;
    expect_still_home(driver, &home, windows).await?;
    fixture.expect_no_requests("the navigation")?;

    // A request for a new window, started by a real click on the page.
    page(driver, POPUP_SCRIPT, vec![json!(fixture.url("/popup"))]).await?;
    ui::visible(driver, POPUP_BUTTON)
        .await?
        .click()
        .await
        .context("clicking the popup button failed")?;
    tokio::time::sleep(SETTLE).await;
    expect_still_home(driver, &home, windows).await?;
    fixture.expect_no_requests("the new-window request")?;
    page(
        driver,
        "document.querySelector(arguments[0])?.remove(); return true;",
        vec![json!(POPUP_BUTTON)],
    )
    .await?;

    // The window still works after both refusals.
    ui::visible(driver, RAIL_ADD_PROJECT)
        .await?
        .click()
        .await
        .context("clicking the add-project button after the refusals failed")?;
    ui::visible(driver, WIZARD)
        .await
        .context("the project wizard did not open after the refusals")?;
    expect_still_home(driver, &home, windows).await
}

/// The window is where it was and is still the only window.
async fn expect_still_home(driver: &WebDriver, home: &str, windows: usize) -> Result<()> {
    let now = driver.current_url().await?.to_string();
    if now != home {
        bail!("the main window moved from {home} to {now}");
    }
    let open = driver.windows().await?.len();
    if open != windows {
        bail!("the application has {open} windows, not the {windows} it started with");
    }
    Ok(())
}

/// A stand-in for an external site. It records the request line of every
/// connection and serves the same page to each one.
struct Fixture {
    address: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("could not bind the external fixture")?;
        let address = listener.local_addr()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8192];
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    let line = String::from_utf8_lossy(&buffer[..read])
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    seen.lock().unwrap_or_else(|e| e.into_inner()).push(line);
                    let body = "<!doctype html><title>External fixture</title><p>External fixture</p>";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Ok(Self {
            address,
            requests,
            task,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    /// Fails with the first request the fixture received, if it received any.
    fn expect_no_requests(&self, step: &str) -> Result<()> {
        let requests = self.requests.lock().unwrap_or_else(|e| e.into_inner());
        match requests.first() {
            Some(line) => bail!("{step} reached the external fixture: {line}"),
            None => Ok(()),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
