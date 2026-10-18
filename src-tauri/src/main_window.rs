//! The main window and the boundary that keeps it on the application's own origin.
//!
//! Tauri creates windows declared in `tauri.conf.json` before `setup()` runs,
//! and it can attach a navigation handler only to a window built in code. The
//! window is therefore built here, from the same declaration, with the guard
//! attached. Nothing else creates a window with this label.

use tauri::utils::config::WindowConfig;
use tauri::webview::{NewWindowResponse, WebviewWindowBuilder};
use tauri::{App, Url, WebviewWindow};

/// The window as it was declared in `tauri.conf.json`, unchanged.
const WINDOW_CONFIG: &str = r#"{
    "label": "main",
    "title": "SlashIt",
    "width": 1280,
    "height": 800,
    "minWidth": 900,
    "minHeight": 600,
    "center": true,
    "decorations": false
}"#;

/// Builds the main window with the navigation boundary attached.
///
/// Top-level navigation is allowed only to the application origin. Every
/// other destination is refused, and nothing is opened on its behalf. External
/// web links leave the window only through the opener, which the frontend
/// calls after a click. A request for a new window is refused outright.
pub fn create(app: &App) -> tauri::Result<WebviewWindow> {
    let config: WindowConfig =
        serde_json::from_str(WINDOW_CONFIG).expect("main window configuration is valid");
    let origin = app_origin(app.config().build.dev_url.as_ref());
    WebviewWindowBuilder::from_config(app, &config)?
        .on_navigation(move |target| is_app_navigation(target, &origin))
        .on_new_window(|_url, _features| NewWindowResponse::Deny)
        .build()
}

/// The origin the application's own content is served from: the dev server in
/// a development build, and the embedded-asset origin otherwise.
fn app_origin(dev_url: Option<&Url>) -> Url {
    app_origin_for(tauri::is_dev(), dev_url, cfg!(any(windows, target_os = "android")))
}

fn app_origin_for(is_dev: bool, dev_url: Option<&Url>, windows_style: bool) -> Url {
    if is_dev {
        if let Some(url) = dev_url {
            return url.clone();
        }
    }
    let raw = if windows_style {
        "http://tauri.localhost"
    } else {
        "tauri://localhost"
    };
    Url::parse(raw).expect("embedded asset origin is a valid URL")
}

/// Whether a top-level navigation may stay in the main window. It must match
/// the application origin's scheme, host and port, and carry no user info.
/// Path, query and fragment are free.
fn is_app_navigation(target: &Url, origin: &Url) -> bool {
    target.scheme() == origin.scheme()
        && target.host_str() == origin.host_str()
        && target.port_or_known_default() == origin.port_or_known_default()
        && target.username().is_empty()
        && target.password().is_none()
}

#[cfg(test)]
mod tests {
    use super::{app_origin_for, is_app_navigation, Url};

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn production_origin_follows_the_platform_scheme() {
        assert_eq!(app_origin_for(false, None, false), url("tauri://localhost"));
        assert_eq!(app_origin_for(false, None, true), url("http://tauri.localhost"));
    }

    #[test]
    fn development_origin_is_the_dev_server_only() {
        let dev = url("http://localhost:1420");
        assert_eq!(app_origin_for(true, Some(&dev), false), dev);
        // A development build without a dev server still gets its own origin.
        assert_eq!(app_origin_for(true, None, false), url("tauri://localhost"));
    }

    #[test]
    fn the_application_origin_and_its_paths_stay_in_the_window() {
        let origin = url("tauri://localhost/");
        for target in [
            "tauri://localhost/",
            "tauri://localhost/index.html",
            "tauri://localhost/tasks?id=3#board",
        ] {
            assert!(is_app_navigation(&url(target), &origin), "{target}");
        }
        let dev = url("http://localhost:1420/");
        assert!(is_app_navigation(&url("http://localhost:1420/board"), &dev));
    }

    #[test]
    fn everything_else_is_refused() {
        let origin = url("tauri://localhost/");
        for target in [
            "https://github.com/BarraDev/slashit",
            "http://github.com/",
            "mailto:someone@example.com",
            "javascript:alert(1)",
            "data:text/html,x",
            "about:blank",
            "file:///etc/passwd",
            "tauri://attacker.example/",
            "tauri://localhost:8080/",
            "tauri://user:secret@localhost/",
        ] {
            assert!(!is_app_navigation(&url(target), &origin), "{target}");
        }
    }

    #[test]
    fn the_dev_origin_is_exact() {
        let dev = url("http://localhost:1420/");
        for target in [
            "http://localhost:1421/",
            "http://localhost/",
            "https://localhost:1420/",
            "http://127.0.0.1:1420/",
            "http://localhost.evil.example:1420/",
            "tauri://localhost/",
        ] {
            assert!(!is_app_navigation(&url(target), &dev), "{target}");
        }
    }

    #[test]
    fn the_windows_origin_is_exact() {
        let origin = url("http://tauri.localhost/");
        assert!(is_app_navigation(&url("http://tauri.localhost/x"), &origin));
        assert!(!is_app_navigation(&url("https://tauri.localhost/x"), &origin));
        assert!(!is_app_navigation(&url("tauri://localhost/"), &origin));
    }
}
