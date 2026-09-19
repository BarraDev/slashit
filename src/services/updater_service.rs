//! Frontend IPC for the Tauri auto-updater commands.
//!
//! Uses the fallible `raw_invoke` binding rather than the `-> JsValue` one that
//! the older services use: an update that failed to download, or a manifest
//! that failed signature verification, must be able to say so. The other
//! binding has no rejection path and would report success for a rejected
//! promise — the one thing an updater must never do.

use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke)]
    fn raw_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;

    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "event"], js_name = "listen")]
    fn tauri_event_listen(event: &str, handler: &Closure<dyn Fn(JsValue)>) -> js_sys::Promise;
}

async fn invoke(cmd: &str, args: JsValue) -> Result<JsValue, String> {
    JsFuture::from(raw_invoke(cmd, args))
        .await
        .map_err(js_error_to_string)
}

fn js_error_to_string(value: JsValue) -> String {
    value.as_string().unwrap_or_else(|| {
        js_sys::JSON::stringify(&value)
            .ok()
            .and_then(|s| s.as_string())
            .unwrap_or_else(|| "Tauri command failed".to_string())
    })
}

/// A release the backend considers newer than the running build.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// Result of one `updater_check`. `update: None` is a real answer — it means
/// the check succeeded and this build is current — so it must never be
/// rendered the same way as "not checked yet".
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub current_version: String,
    #[serde(default)]
    pub update: Option<UpdateInfo>,
    pub checked_at: String,
}

/// Whether this particular build is even capable of updating itself, and
/// whether configuration allows it. Read before any install affordance is
/// rendered: an install button on a build that cannot install is a lie.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterStatus {
    pub current_version: String,
    pub supported: bool,
    #[serde(default)]
    pub unsupported_reason: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub last_check: Option<String>,
}

/// Payload of the `updater://progress` event.
///
/// The field is `content_length`, not `contentLength`, on purpose:
/// `#[serde(rename_all = "camelCase")]` on an enum renames the *variants*, not
/// the fields of its struct variants. This mirrors the backend derive exactly —
/// "fixing" either side alone silently breaks decoding and the bar would sit
/// at zero for the whole download.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", tag = "phase")]
pub enum UpdaterProgress {
    Started {
        #[serde(default)]
        content_length: Option<u64>,
    },
    Chunk {
        downloaded: u64,
        #[serde(default)]
        content_length: Option<u64>,
    },
    Finished,
}

/// Which operation produced a failure. Determines both the wording and which
/// retry the user is offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdaterStage {
    Status,
    Check,
    Install,
    Restart,
}

impl UpdaterStage {
    pub fn label(&self) -> &'static str {
        match self {
            UpdaterStage::Status => "Update status",
            UpdaterStage::Check => "Update check",
            UpdaterStage::Install => "Update install",
            UpdaterStage::Restart => "Restart",
        }
    }
}

/// The distinct failure modes the UI has to tell apart. A signature failure and
/// a flaky network are not the same event and must not read the same: one is a
/// retry, the other is a reason to stop and not install anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdaterErrorKind {
    Network,
    Metadata,
    Signature,
    Unsupported,
    Disabled,
    Stalled,
    Unknown,
}

impl UpdaterErrorKind {
    pub fn headline(&self) -> &'static str {
        match self {
            UpdaterErrorKind::Network => "Could not reach the update server",
            UpdaterErrorKind::Metadata => "The update manifest could not be read",
            UpdaterErrorKind::Signature => "The update failed signature verification",
            UpdaterErrorKind::Unsupported => "This build cannot update itself",
            UpdaterErrorKind::Disabled => "Updates are turned off for this build",
            UpdaterErrorKind::Stalled => "The update stopped reporting progress",
            UpdaterErrorKind::Unknown => "The update did not complete",
        }
    }

    pub fn hint(&self) -> &'static str {
        match self {
            UpdaterErrorKind::Network => {
                "Check your connection and try again. Nothing was downloaded or installed."
            }
            UpdaterErrorKind::Metadata => {
                "The release feed returned something SlashIt could not parse. Nothing was installed."
            }
            UpdaterErrorKind::Signature => {
                "The downloaded package is not signed by the SlashIt release key and was discarded. Do not install it manually; report this."
            }
            UpdaterErrorKind::Unsupported => {
                "Update this installation the way it was installed (package manager, or a fresh download)."
            }
            UpdaterErrorKind::Disabled => {
                "In-app updates are disabled by configuration for this installation."
            }
            UpdaterErrorKind::Stalled => {
                "It may still be running in the background. Give it a moment, then check again."
            }
            UpdaterErrorKind::Unknown => {
                "The running version is unchanged. Try again, or check the release page."
            }
        }
    }

    /// Whether retrying could plausibly succeed. A signature failure or an
    /// unsupported build gets no retry button — retrying is not the answer.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            UpdaterErrorKind::Network
                | UpdaterErrorKind::Metadata
                | UpdaterErrorKind::Stalled
                | UpdaterErrorKind::Unknown
        )
    }
}

/// Map a backend error string onto a user-facing category.
///
/// The backend classifies with a `kind: detail` prefix convention. That file
/// (`src-tauri/src/commands/updater.rs`) did not exist yet when this was
/// written, so the prefix match is backed by substring matching over the whole
/// message: an unrecognised prefix degrades to a best guess rather than to a
/// blank error. Order matters — "invalid signature" contains "invalid", so
/// signature is tested before metadata.
pub fn classify_error(raw: &str) -> UpdaterErrorKind {
    let lower = raw.to_ascii_lowercase();

    let prefix = lower.split(':').next().unwrap_or_default().trim();
    match prefix {
        "network" | "offline" | "connection" | "http" => return UpdaterErrorKind::Network,
        "metadata" | "manifest" | "parse" | "decode" => return UpdaterErrorKind::Metadata,
        "signature" | "verify" | "verification" => return UpdaterErrorKind::Signature,
        "unsupported" => return UpdaterErrorKind::Unsupported,
        // The backend reports an updater it could not even construct — no
        // endpoints, or an architecture the plugin does not build for. From the
        // user's side that is indistinguishable from an unupdatable build, and
        // the "this build cannot update itself" headline is the honest one.
        "not-configured" => return UpdaterErrorKind::Unsupported,
        "disabled" => return UpdaterErrorKind::Disabled,
        "stalled" => return UpdaterErrorKind::Stalled,
        // Named explicitly rather than left to the substring fallback, so a
        // reworded backend message cannot silently reclassify them. All three
        // mean "it did not finish", which is what `Unknown` says.
        "install" | "internal" | "no-update" => return UpdaterErrorKind::Unknown,
        _ => {}
    }

    if lower.contains("signature")
        || lower.contains("verif")
        || lower.contains("pubkey")
        || lower.contains("public key")
        || lower.contains("minisign")
    {
        UpdaterErrorKind::Signature
    } else if lower.contains("unsupported")
        || lower.contains("not supported")
        // `tauri_plugin_updater::Error::TargetNotFound`: the release exists but
        // publishes no artifact for this platform. Retrying cannot fix that.
        || lower.contains("platform object")
    {
        UpdaterErrorKind::Unsupported
    } else if lower.contains("disabled") || lower.contains("turned off") {
        UpdaterErrorKind::Disabled
    } else if lower.contains("parse")
        || lower.contains("deserial")
        || lower.contains("malformed")
        || lower.contains("json")
        || lower.contains("expected value")
        || lower.contains("manifest")
        || lower.contains("updater format")
    {
        UpdaterErrorKind::Metadata
    } else if lower.contains("network")
        || lower.contains("connect")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("dns")
        || lower.contains("unreachable")
        || lower.contains("certificate")
        || lower.contains("tls")
    {
        UpdaterErrorKind::Network
    } else {
        UpdaterErrorKind::Unknown
    }
}

/// A failure with enough context to be rendered honestly: what was being done,
/// what kind of problem it was, and the raw backend text for a bug report.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdaterFailure {
    pub stage: UpdaterStage,
    pub kind: UpdaterErrorKind,
    pub detail: String,
}

impl UpdaterFailure {
    pub fn new(stage: UpdaterStage, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let kind = classify_error(&detail);
        Self {
            stage,
            kind,
            detail,
        }
    }
}

fn empty_args() -> JsValue {
    // An empty argument object cannot fail to serialise, so this is infallible
    // in practice; JsValue::NULL is a harmless fallback rather than a panic.
    serde_wasm_bindgen::to_value(&serde_json::json!({})).unwrap_or(JsValue::NULL)
}

/// Preferred source for the running version: it also reports whether this build
/// can update at all, which `updater_current_version` does not.
pub async fn updater_status() -> Result<UpdaterStatus, String> {
    let response = invoke("updater_status", empty_args()).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

pub async fn updater_check() -> Result<UpdateCheckResult, String> {
    let response = invoke("updater_check", empty_args()).await?;
    serde_wasm_bindgen::from_value(response).map_err(|e| e.to_string())
}

/// Downloads and stages the update. Since the backend stopped calling
/// `app.restart()` itself, `Ok(())` means "installed, not yet running" — the
/// caller must ask the user before restarting.
pub async fn updater_download_and_install() -> Result<(), String> {
    invoke("updater_download_and_install", empty_args())
        .await
        .map(|_| ())
}

/// Replaces the running process. On success this never resolves, so an `Ok`
/// return is not evidence that the restart happened.
pub async fn updater_restart() -> Result<(), String> {
    invoke("updater_restart", empty_args()).await.map(|_| ())
}

/// Subscribe to backend `updater://progress` events. The closure is leaked for
/// the lifetime of the page, so call this exactly once — at app mount, not from
/// the banner, which can be unmounted mid-download.
pub fn subscribe_updater_progress<F: Fn(UpdaterProgress) + 'static>(handler: F) {
    let cb = Closure::wrap(Box::new(move |event: JsValue| {
        // Tauri delivers `{ event, id, payload }` — pull `payload` and decode.
        let payload =
            js_sys::Reflect::get(&event, &JsValue::from_str("payload")).unwrap_or(JsValue::NULL);
        match serde_wasm_bindgen::from_value::<UpdaterProgress>(payload) {
            Ok(ev) => handler(ev),
            Err(e) => leptos::logging::warn!("[updater] bad progress payload: {:?}", e),
        }
    }) as Box<dyn Fn(JsValue)>);
    let promise = tauri_event_listen("updater://progress", &cb);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = JsFuture::from(promise).await;
    });
    cb.forget();
}

/// localStorage key holding a version the user chose to skip permanently.
pub const SKIPPED_VERSION_STORAGE_KEY: &str = "slashit_skipped_update_version";

fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window().and_then(|w| w.local_storage().ok().flatten())
}

pub fn get_skipped_version() -> Option<String> {
    local_storage()
        .and_then(|s| s.get_item(SKIPPED_VERSION_STORAGE_KEY).ok().flatten())
        .filter(|v| !v.is_empty())
}

pub fn set_skipped_version(version: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(SKIPPED_VERSION_STORAGE_KEY, version);
    }
}

pub fn clear_skipped_version() {
    if let Some(storage) = local_storage() {
        let _ = storage.remove_item(SKIPPED_VERSION_STORAGE_KEY);
    }
}

/// Human-readable byte count for download progress.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_convention_wins() {
        assert_eq!(
            classify_error("network: dns failure"),
            UpdaterErrorKind::Network
        );
        assert_eq!(
            classify_error("signature: bad key"),
            UpdaterErrorKind::Signature
        );
        assert_eq!(
            classify_error("disabled: policy"),
            UpdaterErrorKind::Disabled
        );
    }

    #[test]
    fn signature_beats_metadata_on_invalid() {
        // "invalid signature" contains "invalid"; it is not a parse problem.
        assert_eq!(
            classify_error("invalid signature for bundle"),
            UpdaterErrorKind::Signature
        );
    }

    /// The prefix convention is the contract, but a backend that simply
    /// forwards `tauri_plugin_updater::Error` must still land somewhere useful.
    #[test]
    fn raw_plugin_errors_degrade_sensibly() {
        assert_eq!(
            classify_error(
                "error sending request for url (...): error trying to connect: dns error"
            ),
            UpdaterErrorKind::Network
        );
        assert_eq!(
            classify_error("Could not fetch a valid release JSON"),
            UpdaterErrorKind::Metadata
        );
        assert_eq!(
            classify_error("could not find `linux-x86_64` in the platform object"),
            UpdaterErrorKind::Unsupported
        );
    }

    #[test]
    fn unknown_is_not_reported_as_success() {
        assert_eq!(classify_error("something odd"), UpdaterErrorKind::Unknown);
    }
}
