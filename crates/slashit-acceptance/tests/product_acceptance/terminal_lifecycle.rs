// SPDX-License-Identifier: MPL-2.0
//! What happens to a terminal's shell when the window is hidden, when the
//! terminal is closed, and when the application quits. Each terminal's shell
//! ignores SIGHUP and starts a background job, so only a process-group end
//! reaches both. Hiding keeps them running; closing a terminal or quitting ends
//! them.

use super::*;
use std::fs;

const NAV_AGENT: &str = "[data-testid=\"nav-agent\"]";
const NEW_TERMINAL: &str = "[data-testid=\"new-terminal-button\"]";
const CLOSE_TERMINAL: &str = "[data-testid=\"terminal-close\"]";
/// Hides the window through the same close request as its title-bar button.
const HIDE_WINDOW: &str = "window.__TAURI__.window.getCurrentWindow().close(); return true;";
const SHOW_WINDOW: &str = "window.__TAURI__.window.getCurrentWindow().show(); return true;";
/// The call the quit dialog's "Quit Anyway" button makes.
const QUIT_ANYWAY: &str = "window.__TAURI__.core.invoke('force_quit'); return true;";

/// A shell that ignores SIGHUP, starts a job that inherits the ignore, and
/// records both process ids in the file named by `HUP_MARKER`.
const HANGUP_SHELL: &str = "#!/bin/sh\ntrap '' HUP\nsleep 4000 &\necho $$ $! > \"$HUP_MARKER\"\nwhile :; do sleep 1; done\n";

/// Whether `pid` is a live process. A zombie is not live: it has ended and
/// only waits to be reaped.
fn is_live(pid: i32) -> bool {
    // Safety: signal 0 sends nothing; it only asks whether the pid exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !matches!(
            stat.rsplit(')').next().and_then(|rest| rest.trim_start().chars().next()),
            Some('Z' | 'X')
        ),
        Err(_) => false,
    }
}

async fn create_project(driver: &WebDriver, name: &str) -> Result<String> {
    created_id(ui::invoke(driver, "create_project", json!({
        "name": name, "repositoryId": Value::Null, "agentType": "claude_code"
    })).await?, "create_project")
}

/// Reads the shell and job pids the newest terminal's shell has recorded.
async fn terminal_pids(marker: &Path) -> Result<(i32, i32)> {
    let started = Instant::now();
    loop {
        let parsed = fs::read_to_string(marker).ok().and_then(|text| {
            let mut parts = text.split_whitespace().map(|p| p.parse::<i32>());
            match (parts.next(), parts.next()) {
                (Some(Ok(shell)), Some(Ok(job))) => Some((shell, job)),
                _ => None,
            }
        });
        if let Some(pair) = parsed {
            return Ok(pair);
        }
        if started.elapsed() > ui::ELEMENT_TIMEOUT {
            bail!("the terminal's shell did not record its processes");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The processes among `pids` that are live now.
fn live_among(pids: [i32; 2]) -> Vec<i32> {
    pids.into_iter().filter(|pid| is_live(*pid)).collect()
}

/// Waits until none of `pids` is live, or until `limit` has passed, and returns
/// whichever are still live.
async fn survivors_after_wait(pids: [i32; 2], limit: Duration) -> Vec<i32> {
    let started = Instant::now();
    loop {
        let alive = live_among(pids);
        if alive.is_empty() || started.elapsed() > limit {
            return alive;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Opens a new terminal in the Agent Terminals page and returns its processes.
/// The marker is cleared first, so the pids read are the new shell's.
async fn open_terminal(driver: &WebDriver, marker: &Path) -> Result<[i32; 2]> {
    let _ = fs::remove_file(marker);
    ui::visible(driver, NEW_TERMINAL).await?
        .click().await.context("could not create a terminal")?;
    let (shell, job) = terminal_pids(marker).await?;
    Ok([shell, job])
}

#[tokio::test(flavor = "multi_thread")]
async fn hiding_keeps_a_terminal_running_and_closing_or_quitting_ends_it() {
    let context = TestContext::new("terminal_lifecycle").expect("harness setup");
    let shell_path = context.state().path().join("hangup-shell.sh");
    fs::write(&shell_path, HANGUP_SHELL).expect("write the shell");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shell_path, fs::Permissions::from_mode(0o755)).expect("make the shell executable");
    }
    let marker = context.state().path().join("hangup-marker");
    context.set_child_env("SHELL", shell_path.as_os_str().to_os_string());
    context.set_child_env("HUP_MARKER", marker.as_os_str().to_os_string());

    let outcome = async {
        let session = context.start_session("terminal-lifecycle").await?;
        let driver = session.driver();
        create_project(driver, "Terminal Lifecycle").await?;
        driver.refresh().await.context("could not reload the application window")?;
        ui::assert_frontend_is_real(driver).await?;
        ui::visible(driver, NAV_AGENT).await?
            .click().await.context("could not open the Agent Terminals page")?;

        // Hiding the window keeps the terminal's shell and its job.
        let hidden = open_terminal(driver, &marker).await?;
        page(driver, HIDE_WINDOW, vec![]).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        if live_among(hidden).len() != hidden.len() {
            bail!("hiding the window ended the terminal: {:?} of {hidden:?} are still running", live_among(hidden));
        }
        page(driver, SHOW_WINDOW, vec![]).await?;
        ui::visible(driver, NAV_AGENT).await?;

        // Closing the terminal ends its shell and its job.
        ui::visible(driver, CLOSE_TERMINAL).await?
            .click().await.context("could not close the terminal")?;
        let after_close = survivors_after_wait(hidden, Duration::from_secs(5)).await;
        if !after_close.is_empty() {
            bail!("closing the terminal left {after_close:?} running");
        }

        // Quitting ends a terminal whose shell ignores SIGHUP, and its job.
        let quitting = open_terminal(driver, &marker).await?;
        page(driver, QUIT_ANYWAY, vec![]).await?;
        let after_quit = survivors_after_wait(quitting, Duration::from_secs(10)).await;
        if !after_quit.is_empty() {
            bail!("quitting the application left {after_quit:?} running");
        }

        Ok::<_, anyhow::Error>(())
    }
    .await;
    context.finish(outcome);
}
