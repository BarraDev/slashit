//! Rust-only desktop acceptance harness for SlashIt.
//!
//! The journeys in `tests/acceptance.rs` drive the real desktop application:
//! a real Tauri process, the real Leptos/WASM frontend and the real Rust
//! backend, against a private state root, over the W3C WebDriver protocol.
//! There is no Node, npm, Bun or WebdriverIO anywhere in the loop — the
//! client is [`thirtyfour`] and the provider is the official `tauri-driver`.
//!
//! # Running the journeys
//!
//! ```text
//! cargo tauri build --debug --no-bundle
//! cargo test -p slashit-acceptance --features run-acceptance
//! ```
//!
//! The build step is not optional and not something a test should do for you:
//! a plain `cargo build` produces a binary that loads `devUrl`
//! ([`DEV_URL_PREFIX`]) and shows a connection-error page unless a Trunk
//! server happens to be running. `cargo tauri build` enables the
//! `custom-protocol` feature, which embeds `dist/` and makes the application
//! load `tauri://localhost` with no server at all.
//! [`ui::assert_frontend_is_real`] asserts that difference rather than
//! trusting it.
//!
//! # Platform support
//!
//! **The acceptance runtime is Linux-only today, and the module layout says
//! so rather than leaving it to a comment.** [`ports`], [`diagnostics`] and
//! [`ui`] speak only the WebDriver protocol and compile everywhere. Every
//! module that actually starts, isolates or tears down a desktop application
//! — [`driver`], [`process`], [`state`] and [`context`] — is
//! `#[cfg(target_os = "linux")]`, because the implementations behind them are
//! Unix process groups, POSIX signals, `/proc` and XDG directories.
//!
//! macOS and Windows are therefore *unimplemented*, not merely untested. A
//! future macOS provider (`thirtyfour` against the embedded
//! `tauri-plugin-wdio-webdriver` server) and a future Windows one
//! (`tauri-driver` against `msedgedriver`, with Job Objects instead of
//! process groups) get their own `#[cfg]` implementations of the same module
//! names, and reuse the portable half unchanged. Until then the non-Linux
//! compile check proves the boundary holds: it fails the moment a `/proc`
//! read or a `libc::kill` escapes its gate.
//!
//! # Configuration
//!
//! | Variable | Meaning |
//! |---|---|
//! | `SLASHIT_ACCEPTANCE_BIN` | The application under test. Defaults to `target/debug/slashit-ui`. |
//! | `SLASHIT_ACCEPTANCE_NATIVE_DRIVER` | Path to `WebKitWebDriver`. Optional; `tauri-driver` finds it on `PATH` otherwise. |

// Portable: these carry no process, filesystem-layout or platform assumption
// beyond "there is a WebDriver endpoint and a TCP stack".
pub mod diagnostics;
pub mod ports;
pub mod ui;

// The acceptance runtime. See the platform note above before adding to this
// list -- anything here is a promise that the platform is implemented.
#[cfg(target_os = "linux")]
pub mod context;
#[cfg(target_os = "linux")]
pub mod driver;
#[cfg(target_os = "linux")]
pub mod fake_agent;
#[cfg(target_os = "linux")]
pub mod process;
#[cfg(target_os = "linux")]
pub mod state;

#[cfg(target_os = "linux")]
pub use context::{Environment, TestContext};
#[cfg(target_os = "linux")]
pub use driver::Session;
#[cfg(target_os = "linux")]
pub use fake_agent::FakeAgent;
#[cfg(target_os = "linux")]
pub use state::StateRoot;

/// The URL a binary built *without* `custom-protocol` loads: `devUrl` from
/// `src-tauri/tauri.conf.json`.
///
/// Two guards depend on this value — the fast-fail in
/// [`driver::Session`] and the semantic assertion in
/// [`ui::assert_frontend_is_real`] — so it lives in one place. If they drifted
/// apart, a development build would slip past the cheap check and be
/// diagnosed as a mount timeout a minute later instead.
///
/// `dev_url_matches_the_tauri_configuration` below keeps the copy honest
/// without the harness having to parse the configuration at runtime.
pub const DEV_URL_PREFIX: &str = "http://localhost:1420";

/// The element that proves the Leptos application mounted, as opposed to the
/// window merely existing.
pub const ROOT_ELEMENT: &str = "[data-testid=\"sidebar\"]";

#[cfg(test)]
mod tests {
    use super::*;

    /// The one thing a duplicated constant needs: something that fails when
    /// the original moves.
    #[test]
    fn dev_url_matches_the_tauri_configuration() {
        let config_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("walk up to the workspace root")
            .join("src-tauri/tauri.conf.json");

        let raw = std::fs::read_to_string(&config_path)
            .unwrap_or_else(|e| panic!("could not read {}: {e}", config_path.display()));
        let config: serde_json::Value =
            serde_json::from_str(&raw).expect("tauri.conf.json is not valid JSON");

        let dev_url = config["build"]["devUrl"]
            .as_str()
            .expect("tauri.conf.json has no build.devUrl");

        assert!(
            dev_url.starts_with(DEV_URL_PREFIX),
            "devUrl is now {dev_url:?}, so DEV_URL_PREFIX ({DEV_URL_PREFIX:?}) no longer \
             recognises a development build"
        );
    }
}
