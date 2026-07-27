//! Self-update: what this install can do, what the release channel offers, and
//! who decides when the process dies.
//!
//! Three things here are deliberate and none of them are obvious.
//!
//! **The install is inspected before anything is downloaded.** Not every copy
//! of SlashIt can replace itself. A `.deb`, an `.rpm` and a bare binary out of
//! `target/` all run perfectly well and all fail at the last step, after the
//! payload is on disk — and on an unpackaged build the plugin's Linux fallback
//! rewrites whatever `current_exe()` points at, which during development is the
//! developer's own build output. [`unsupported_reason`] answers "can this
//! install update itself" up front so the UI never offers a button that ends in
//! a corrupted binary.
//!
//! **Failures are classified, not stringified.** Every error this module
//! returns is `"<code>: <sentence>"`, where the code is one of
//! [`UpdaterErrorKind::code`]. "The signature did not verify" and "the network
//! is down" are the same `Err(String)` on the wire, and the frontend has to tell
//! them apart to say anything useful.
//!
//! **Nothing here restarts the application.** SlashIt holds running agents and
//! live PTYs; killing the process to swap a binary would discard both without
//! asking. The install ends by emitting [`UpdaterProgress::Finished`] and
//! stopping. The frontend then does what it already does before a quit — count
//! active processes, show the confirmation dialog — and calls
//! [`updater_restart`] once the user agrees.
//!
//! Progress leaves through the [`EventSink`](crate::events::EventSink) rather
//! than an `AppHandle`, so the flow is not tied to a webview existing.

use crate::events::EventSinkExt;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;
use tauri::utils::config::BundleType;
use tauri::AppHandle;
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

/// The runtime flag that gates every command in this module.
///
/// Registered in [`crate::config::features::REGISTRY`]. A build that does not
/// know the id is treated as having updates switched off, because a flag that
/// cannot be read cannot be trusted to be on.
pub const AUTO_UPDATE_FLAG: &str = "auto_update";

/// The event name every [`UpdaterProgress`] is emitted under.
pub const PROGRESS_EVENT: &str = "updater://progress";

// --- State ------------------------------------------------------------------

/// What the updater remembers between IPC calls.
///
/// Both fields are read back out: `pending` saves a second round trip to the
/// release endpoint when the user accepts an update they were just shown, and
/// `last_check` is returned by [`updater_status`] so the timestamp survives a
/// webview reload rather than being recomputed as "never".
#[derive(Clone, Default)]
pub struct UpdaterState {
    /// The update the last successful check found, if it found one.
    pending: Arc<Mutex<Option<Update>>>,
    /// When the last *successful* check completed.
    last_check: Arc<Mutex<Option<DateTime<Utc>>>>,
}

impl UpdaterState {
    pub fn new() -> Self {
        Self::default()
    }
}

// --- Wire types -------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub date: Option<String>,
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub update: Option<UpdateInfo>,
    pub checked_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterStatus {
    pub current_version: String,
    /// Whether this install can actually replace itself.
    pub supported: bool,
    /// Why not, in a sentence the UI can show verbatim.
    pub unsupported_reason: Option<String>,
    /// The feature flag, resolved. Independent of `supported`: a capable
    /// install with the flag off is still not going to update.
    pub enabled: bool,
    /// RFC 3339. `None` until a check succeeds in this process.
    pub last_check: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "phase")]
pub enum UpdaterProgress {
    Started {
        content_length: Option<u64>,
    },
    Chunk {
        downloaded: u64,
        content_length: Option<u64>,
    },
    /// The new version is installed and on disk. Emitted only after the install
    /// step returns, never merely when the download ends.
    Finished,
}

// --- Error taxonomy ---------------------------------------------------------

/// The classes of failure the frontend renders differently.
///
/// A signature mismatch means "do not retry, something is wrong with the
/// release"; a network error means "try again later"; a `.deb` install means
/// "there is nothing to retry, use your package manager". Collapsing those into
/// one string forces the UI to guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdaterErrorKind {
    /// The `auto_update` flag is off, or this build does not know it.
    Disabled,
    /// This install cannot replace itself, whatever the release channel says.
    Unsupported,
    /// No usable endpoint: none configured, an unparseable URL, an
    /// architecture or platform the updater does not build for.
    NotConfigured,
    /// The release endpoint could not be reached.
    Network,
    /// The endpoint answered, but the release manifest was missing, malformed,
    /// or had no entry for this platform.
    Metadata,
    /// The payload's minisign signature did not verify.
    Signature,
    /// The check succeeded and there is nothing newer.
    NoUpdate,
    /// The payload arrived and verified, but could not be put in place.
    Install,
    /// Anything the plugin can return that does not fit above.
    Internal,
}

impl UpdaterErrorKind {
    /// The stable token the frontend branches on. Every error string this
    /// module returns is `"<code>: <sentence>"`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unsupported => "unsupported",
            Self::NotConfigured => "not-configured",
            Self::Network => "network",
            Self::Metadata => "metadata",
            Self::Signature => "signature",
            Self::NoUpdate => "no-update",
            Self::Install => "install",
            Self::Internal => "internal",
        }
    }
}

/// Build the wire form of an error: class, then a sentence a user can act on.
fn err(kind: UpdaterErrorKind, detail: &str) -> String {
    format!("{}: {detail}", kind.code())
}

/// Sort a plugin error into the class the UI reacts to.
///
/// The plugin's error enum is `#[non_exhaustive]`, so an unmatched variant
/// lands in [`UpdaterErrorKind::Internal`] rather than breaking the build on a
/// dependency bump.
fn classify(error: &tauri_plugin_updater::Error) -> UpdaterErrorKind {
    use tauri_plugin_updater::Error as E;
    match error {
        E::EmptyEndpoints
        | E::InsecureTransportProtocol
        | E::UnsupportedOs
        | E::UnsupportedArch
        | E::UrlParse(_)
        | E::InvalidHeaderName(_)
        | E::InvalidHeaderValue(_) => UpdaterErrorKind::NotConfigured,

        E::Reqwest(_) | E::Network(_) => UpdaterErrorKind::Network,

        E::ReleaseNotFound
        | E::TargetNotFound(_)
        | E::TargetsNotFound(_)
        | E::Serialization(_)
        | E::Semver(_)
        | E::FormatDate => UpdaterErrorKind::Metadata,

        E::Minisign(_) | E::Base64(_) | E::SignatureUtf8(_) => UpdaterErrorKind::Signature,

        // Everything from "we have verified bytes" onwards: unpacking, moving
        // the binary into place, and asking for the privileges to do so.
        E::Io(_)
        | E::FailedToDetermineExtractPath
        | E::TempDirNotOnSameMountPoint
        | E::TempDirNotFound
        | E::BinaryNotFoundInArchive
        | E::InvalidUpdaterFormat
        | E::AuthenticationFailed
        | E::DebInstallFailed
        | E::PackageInstallFailed => UpdaterErrorKind::Install,

        _ => UpdaterErrorKind::Internal,
    }
}

/// Turn a plugin error into its wire form in one step.
fn from_plugin(error: &tauri_plugin_updater::Error) -> String {
    err(classify(error), &error.to_string())
}

// --- Capability detection ---------------------------------------------------

/// How this process was packaged, as far as replacing itself is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// A single self-contained file the plugin rewrites in place.
    AppImage,
    /// A Debian package; `dpkg` owns the installed files.
    Deb,
    /// An RPM package; `rpm` owns the installed files.
    Rpm,
    /// A macOS `.app`, however it was delivered.
    MacApp,
    /// A Windows MSI or NSIS install.
    WindowsInstaller,
    /// No bundle marker was patched into the binary: run straight out of a
    /// build directory, or copied out of one.
    Unpackaged,
}

/// Everything the capability decision depends on.
///
/// Gathered by [`probe_install`] from the running process and passed as data so
/// that [`unsupported_reason`] can be exercised for every platform from a unit
/// test, on one machine, without a Tauri application.
#[derive(Debug, Clone)]
pub struct InstallProbe {
    /// `cfg!(debug_assertions)` at the call site.
    pub debug_build: bool,
    /// `std::env::consts::OS`.
    pub os: &'static str,
    pub install: InstallKind,
    /// Why `UpdaterExt::updater()` refused to build, when it did. Building it
    /// is offline: it only reads the configured endpoints and resolves the
    /// current executable.
    pub updater_unavailable: Option<String>,
}

/// Read the packaging marker, with the AppImage runtime as a second opinion.
///
/// The marker is patched into the binary by the bundler, so a build that was
/// never bundled reports nothing. An AppImage additionally exports `APPIMAGE`
/// at run time, which is the one case where an unmarked binary can still
/// legitimately replace itself.
fn install_kind(bundle: Option<BundleType>, appimage_env: bool) -> InstallKind {
    match bundle {
        Some(BundleType::AppImage) => InstallKind::AppImage,
        Some(BundleType::Deb) => InstallKind::Deb,
        Some(BundleType::Rpm) => InstallKind::Rpm,
        Some(BundleType::App | BundleType::Dmg) => InstallKind::MacApp,
        Some(BundleType::Msi | BundleType::Nsis) => InstallKind::WindowsInstaller,
        None if appimage_env => InstallKind::AppImage,
        None => InstallKind::Unpackaged,
    }
}

/// The packaged form a user of `os` should install to get updates.
fn packaged_form(os: &str) -> &'static str {
    match os {
        "linux" => "AppImage",
        "macos" => "application bundle",
        "windows" => "installer",
        _ => "packaged",
    }
}

/// Why this install cannot update itself, or `None` when it can.
///
/// Ordered most-fundamental first: an updater that cannot even be constructed
/// makes every later diagnosis moot.
pub fn unsupported_reason(probe: &InstallProbe) -> Option<String> {
    if let Some(why) = &probe.updater_unavailable {
        return Some(format!(
            "The updater is not usable in this build: {why} Updates are unavailable until an endpoint is configured."
        ));
    }

    // A debug build is never updatable, even if it happens to be bundled: the
    // release channel only publishes release builds, so "update" here means
    // replacing a developer's binary with an unrelated one.
    if probe.debug_build {
        return Some(
            "This is a development build. Self-update only replaces released bundles; \
             install a SlashIt release to receive updates."
                .to_string(),
        );
    }

    if !matches!(probe.os, "linux" | "macos" | "windows") {
        return Some(format!(
            "Self-update is not implemented for {}. Rebuild from source to move to a new version.",
            probe.os
        ));
    }

    match probe.install {
        InstallKind::AppImage | InstallKind::MacApp | InstallKind::WindowsInstaller => None,

        // The published update artifact is the AppImage archive, which dpkg
        // and rpm cannot install; the package manager that owns these files is
        // the only thing that can replace them.
        InstallKind::Deb => Some(
            "SlashIt was installed from a .deb package. Update it with apt or dpkg — \
             the published update artifact is an AppImage, which dpkg cannot install."
                .to_string(),
        ),
        InstallKind::Rpm => Some(
            "SlashIt was installed from an .rpm package. Update it with dnf or rpm — \
             the published update artifact is an AppImage, which rpm cannot install."
                .to_string(),
        ),

        InstallKind::Unpackaged => Some(format!(
            "SlashIt is running from an unpackaged binary on {}, so there is nothing for \
             the updater to replace safely. Install the {} build to receive updates.",
            probe.os,
            packaged_form(probe.os)
        )),
    }
}

/// The error a command should return instead of starting work, or `None`.
///
/// Splits "no update source" from "this install cannot apply one", because the
/// first is fixable by configuration and the second is not.
fn capability_refusal(probe: &InstallProbe) -> Option<String> {
    let reason = unsupported_reason(probe)?;
    let kind = if probe.updater_unavailable.is_some() {
        UpdaterErrorKind::NotConfigured
    } else {
        UpdaterErrorKind::Unsupported
    };
    Some(err(kind, &reason))
}

/// Inspect the running process.
fn probe_install(app: &AppHandle) -> InstallProbe {
    InstallProbe {
        debug_build: cfg!(debug_assertions),
        os: std::env::consts::OS,
        install: install_kind(
            tauri::utils::platform::bundle_type(),
            std::env::var_os("APPIMAGE").is_some(),
        ),
        updater_unavailable: app.updater().err().map(|e| e.to_string()),
    }
}

// --- Feature flag -----------------------------------------------------------

/// The error to return when the flag forbids updating, or `None`.
///
/// An unrecognised id reads as off. A build that does not carry the flag has
/// not shipped the feature, and guessing "on" would run an update path the
/// build was never meant to expose.
fn feature_refusal(flag: Option<bool>) -> Option<String> {
    match flag {
        Some(true) => None,
        Some(false) => Some(err(
            UpdaterErrorKind::Disabled,
            &format!(
                "Automatic updates are turned off. Enable the '{AUTO_UPDATE_FLAG}' feature flag in Settings to check for updates."
            ),
        )),
        None => Some(err(UpdaterErrorKind::Disabled, &unknown_flag_reason())),
    }
}

fn unknown_flag_reason() -> String {
    format!(
        "This build does not recognise the '{AUTO_UPDATE_FLAG}' feature flag, so updates cannot be enabled."
    )
}

// --- Status assembly --------------------------------------------------------

/// Assemble the status from already-gathered facts.
///
/// Separate from [`updater_status`] so the precedence between "this install
/// cannot update" and "this build has no such flag" is pinned by a test rather
/// than by whichever branch happened to run first.
fn build_status(
    current_version: String,
    probe: &InstallProbe,
    flag: Option<bool>,
    last_check: Option<DateTime<Utc>>,
) -> UpdaterStatus {
    let install_reason = unsupported_reason(probe);
    let supported = install_reason.is_none();

    // The install reason wins: a `.deb` cannot be made updatable by toggling
    // anything, so reporting the flag instead would send the user to a switch
    // that changes nothing. The unknown-flag reason is what distinguishes "the
    // user turned it off" from "this build never had it".
    let unsupported_reason = install_reason.or_else(|| match flag {
        None => Some(unknown_flag_reason()),
        Some(_) => None,
    });

    UpdaterStatus {
        current_version,
        supported,
        unsupported_reason,
        enabled: flag.unwrap_or(false),
        last_check: last_check.map(|t| t.to_rfc3339()),
    }
}

/// The publication date to show, preferring the manifest's own spelling.
///
/// `Update::date` is a parsed `OffsetDateTime` whose `Display` is not RFC 3339,
/// so handing it to the frontend would ship a timestamp no date parser accepts.
/// The manifest's `pub_date` is the string the release actually published.
fn published_at(raw_json: &serde_json::Value, parsed: Option<String>) -> Option<String> {
    raw_json
        .get("pub_date")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or(parsed)
}

fn update_to_info(update: &Update, current_version: &str) -> UpdateInfo {
    UpdateInfo {
        version: update.version.clone(),
        current_version: current_version.to_string(),
        date: published_at(&update.raw_json, update.date.map(|d| d.to_string())),
        body: update.body.clone(),
    }
}

// --- Commands ---------------------------------------------------------------

/// What this install can do, without touching the network.
///
/// Always `Ok`: "the updater is broken" is a state to render, not a call that
/// failed. `Result` only because Tauri requires it of an async command holding
/// a `State` borrow; `Ok(v)` is serialised as `v`, so the frontend sees the
/// struct either way.
#[tauri::command]
pub async fn updater_status(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<UpdaterStatus, String> {
    let flag = state.features.read().await.get(AUTO_UPDATE_FLAG);
    let last_check = *state.updater.last_check.lock().await;

    Ok(build_status(
        // The same source the plugin compares against, so status and check can
        // never disagree about which version is running.
        app.package_info().version.to_string(),
        &probe_install(&app),
        flag,
        last_check,
    ))
}

/// Ask the release endpoint whether something newer exists.
///
/// Runs even on an install that cannot apply an update: reading the manifest
/// changes nothing on disk, and "0.2.0 is out, install it yourself" is useful
/// to a `.deb` user. [`UpdaterStatus::supported`] is what tells the UI whether
/// to offer the install button.
#[tauri::command]
pub async fn updater_check(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<UpdateCheckResult, String> {
    let flag = state.features.read().await.get(AUTO_UPDATE_FLAG);
    if let Some(refusal) = feature_refusal(flag) {
        return Err(refusal);
    }

    let current_version = app.package_info().version.to_string();
    let updater = app.updater().map_err(|e| from_plugin(&e))?;
    let found = updater.check().await.map_err(|e| from_plugin(&e))?;

    // Recorded only on success. A timestamp that says "checked a minute ago"
    // after a failed check would read as "you are up to date".
    let checked_at = Utc::now();
    *state.updater.last_check.lock().await = Some(checked_at);

    let info = match found {
        Some(update) => {
            let info = update_to_info(&update, &current_version);
            *state.updater.pending.lock().await = Some(update);
            Some(info)
        }
        None => {
            // Clear it: a stale pending update would let the install command
            // apply a version the user was never shown.
            *state.updater.pending.lock().await = None;
            None
        }
    };

    Ok(UpdateCheckResult {
        current_version,
        update: info,
        checked_at: checked_at.to_rfc3339(),
    })
}

/// Download the pending update and put it in place. Does not restart.
///
/// Returns `Ok(())` only once the new version is on disk, and emits
/// [`UpdaterProgress::Finished`] at the same moment. The frontend confirms with
/// the user — it already knows how to count running agents and PTYs — and then
/// calls [`updater_restart`].
#[tauri::command]
pub async fn updater_download_and_install(
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<(), String> {
    let flag = state.features.read().await.get(AUTO_UPDATE_FLAG);
    if let Some(refusal) = feature_refusal(flag) {
        return Err(refusal);
    }

    // Before a single byte moves. On an unpackaged Linux build the plugin's
    // fallback rewrites whatever `current_exe()` resolves to, which in a
    // development tree is the developer's own build output.
    let probe = probe_install(&app);
    if let Some(refusal) = capability_refusal(&probe) {
        return Err(refusal);
    }

    let cached = state.updater.pending.lock().await.take();
    let update = match cached {
        Some(update) => update,
        None => {
            let updater = app.updater().map_err(|e| from_plugin(&e))?;
            updater
                .check()
                .await
                .map_err(|e| from_plugin(&e))?
                .ok_or_else(|| {
                    err(
                        UpdaterErrorKind::NoUpdate,
                        "SlashIt is already on the latest published version.",
                    )
                })?
        }
    };

    let events = state.events();
    events.emit(
        PROGRESS_EVENT,
        &UpdaterProgress::Started {
            content_length: None,
        },
    );

    let chunk_events = state.events();
    let mut downloaded: u64 = 0;
    let outcome = update
        .download_and_install(
            move |chunk_length, content_length| {
                downloaded = downloaded.saturating_add(chunk_length as u64);
                chunk_events.emit(
                    PROGRESS_EVENT,
                    &UpdaterProgress::Chunk {
                        downloaded,
                        content_length,
                    },
                );
            },
            // Deliberately empty. This fires when the *download* ends, with the
            // install still to come; emitting `Finished` here is what made the
            // previous implementation report success for work that had not
            // happened.
            || {},
        )
        .await;

    match outcome {
        Ok(()) => {
            events.emit(PROGRESS_EVENT, &UpdaterProgress::Finished);
            Ok(())
        }
        Err(e) => {
            // Put it back. The update is still the one the last check found,
            // and a dropped connection should not force the user to check
            // again before retrying.
            *state.updater.pending.lock().await = Some(update);
            Err(from_plugin(&e))
        }
    }
}

/// Restart into the installed version.
///
/// Unconditional by design, and therefore only ever called by a frontend that
/// has already asked. Deliberately not gated on the feature flag: once a new
/// binary is on disk, refusing to restart would leave the application running
/// code that no longer matches its own installation.
#[tauri::command]
pub fn updater_restart(app: AppHandle) {
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_plugin_updater::Error as PluginError;

    fn probe(os: &'static str, install: InstallKind) -> InstallProbe {
        InstallProbe {
            debug_build: false,
            os,
            install,
            updater_unavailable: None,
        }
    }

    // --- Capability detection ----------------------------------------------

    #[test]
    fn a_released_bundle_on_every_supported_platform_can_update_itself() {
        for (os, install) in [
            ("linux", InstallKind::AppImage),
            ("macos", InstallKind::MacApp),
            ("windows", InstallKind::WindowsInstaller),
        ] {
            assert_eq!(
                unsupported_reason(&probe(os, install)),
                None,
                "{os}/{install:?} is the shipped form and must be updatable"
            );
        }
    }

    #[test]
    fn a_development_build_is_never_updatable_even_when_bundled() {
        let mut p = probe("linux", InstallKind::AppImage);
        p.debug_build = true;

        let reason = unsupported_reason(&p).expect("a debug build must refuse");
        assert!(reason.contains("development build"), "{reason}");
        // And it is not merely inherited from the packaging check, which this
        // probe passes.
        assert_eq!(
            unsupported_reason(&probe("linux", InstallKind::AppImage)),
            None
        );
    }

    #[test]
    fn a_system_package_is_refused_and_names_the_package_manager() {
        let deb = unsupported_reason(&probe("linux", InstallKind::Deb)).expect("deb must refuse");
        assert!(deb.contains("dpkg"), "{deb}");
        assert!(deb.contains("apt"), "{deb}");

        let rpm = unsupported_reason(&probe("linux", InstallKind::Rpm)).expect("rpm must refuse");
        assert!(rpm.contains("rpm"), "{rpm}");
        assert!(rpm.contains("dnf"), "{rpm}");
    }

    #[test]
    fn an_unpackaged_binary_is_refused_and_names_the_form_to_install() {
        for (os, expected) in [
            ("linux", "AppImage"),
            ("macos", "application bundle"),
            ("windows", "installer"),
        ] {
            let reason = unsupported_reason(&probe(os, InstallKind::Unpackaged))
                .unwrap_or_else(|| panic!("an unpackaged binary on {os} must refuse"));
            assert!(reason.contains(expected), "{os}: {reason}");
        }
    }

    #[test]
    fn a_platform_the_updater_does_not_build_for_is_refused() {
        // The plugin's `updater_os()` knows linux, darwin and windows only; on
        // anything else the check fails after a network round trip.
        let reason = unsupported_reason(&probe("freebsd", InstallKind::Unpackaged))
            .expect("freebsd must refuse");
        assert!(reason.contains("freebsd"), "{reason}");
    }

    #[test]
    fn an_updater_that_cannot_be_built_outranks_every_other_reason() {
        let mut p = probe("linux", InstallKind::AppImage);
        p.debug_build = true;
        p.updater_unavailable = Some("Updater does not have any endpoints set.".to_string());

        let reason = unsupported_reason(&p).expect("an unusable updater must refuse");
        assert!(reason.contains("endpoints"), "{reason}");
        assert!(
            !reason.contains("development build"),
            "the deeper cause must win: {reason}"
        );

        // And that ordering is what makes the class correct, since a missing
        // endpoint is configuration rather than packaging.
        let refusal = capability_refusal(&p).unwrap();
        assert!(refusal.starts_with("not-configured: "), "{refusal}");
    }

    #[test]
    fn a_capability_refusal_for_packaging_is_classed_unsupported() {
        let refusal = capability_refusal(&probe("linux", InstallKind::Deb)).unwrap();
        assert!(refusal.starts_with("unsupported: "), "{refusal}");
        assert_eq!(
            capability_refusal(&probe("linux", InstallKind::AppImage)),
            None
        );
    }

    #[test]
    fn every_bundle_marker_maps_to_an_install_kind() {
        assert_eq!(
            install_kind(Some(BundleType::AppImage), false),
            InstallKind::AppImage
        );
        assert_eq!(install_kind(Some(BundleType::Deb), false), InstallKind::Deb);
        assert_eq!(install_kind(Some(BundleType::Rpm), false), InstallKind::Rpm);
        assert_eq!(
            install_kind(Some(BundleType::App), false),
            InstallKind::MacApp
        );
        assert_eq!(
            install_kind(Some(BundleType::Dmg), false),
            InstallKind::MacApp
        );
        assert_eq!(
            install_kind(Some(BundleType::Msi), false),
            InstallKind::WindowsInstaller
        );
        assert_eq!(
            install_kind(Some(BundleType::Nsis), false),
            InstallKind::WindowsInstaller
        );
    }

    #[test]
    fn an_unmarked_binary_is_an_appimage_only_when_the_runtime_says_so() {
        // The bundler patches no marker into a `cargo build` binary, so the
        // `APPIMAGE` variable the AppImage runtime exports is the only
        // remaining evidence.
        assert_eq!(install_kind(None, true), InstallKind::AppImage);
        assert_eq!(install_kind(None, false), InstallKind::Unpackaged);

        // A marker present always wins: `APPIMAGE` inherited from an unrelated
        // parent process must not turn a .deb into an AppImage.
        assert_eq!(install_kind(Some(BundleType::Deb), true), InstallKind::Deb);
    }

    // --- Error classification ----------------------------------------------

    #[test]
    fn every_class_has_a_distinct_non_empty_code() {
        let kinds = [
            UpdaterErrorKind::Disabled,
            UpdaterErrorKind::Unsupported,
            UpdaterErrorKind::NotConfigured,
            UpdaterErrorKind::Network,
            UpdaterErrorKind::Metadata,
            UpdaterErrorKind::Signature,
            UpdaterErrorKind::NoUpdate,
            UpdaterErrorKind::Install,
            UpdaterErrorKind::Internal,
        ];
        let mut codes: Vec<&str> = kinds.iter().map(|k| k.code()).collect();
        codes.sort_unstable();
        let unique = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), unique, "two classes share a code: {codes:?}");
        assert!(codes.iter().all(|c| !c.is_empty() && !c.contains(' ')));
    }

    #[test]
    fn an_error_string_is_its_class_then_a_sentence() {
        // The whole convention the frontend parses: split once on ": ".
        let message = err(UpdaterErrorKind::Signature, "The signature did not verify.");
        assert_eq!(
            message.split_once(": "),
            Some(("signature", "The signature did not verify."))
        );
    }

    #[test]
    fn endpoint_and_platform_problems_classify_as_configuration() {
        for error in [
            PluginError::EmptyEndpoints,
            PluginError::InsecureTransportProtocol,
            PluginError::UnsupportedOs,
            PluginError::UnsupportedArch,
        ] {
            assert_eq!(
                classify(&error),
                UpdaterErrorKind::NotConfigured,
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_failed_download_classifies_as_network() {
        let error = PluginError::Network("connection reset".to_string());
        assert_eq!(classify(&error), UpdaterErrorKind::Network);
        assert!(from_plugin(&error).starts_with("network: "));
    }

    #[test]
    fn an_unusable_manifest_classifies_as_metadata() {
        for error in [
            PluginError::ReleaseNotFound,
            PluginError::TargetNotFound("linux-x86_64".to_string()),
            PluginError::TargetsNotFound(vec!["linux-x86_64".to_string()]),
            PluginError::FormatDate,
        ] {
            assert_eq!(classify(&error), UpdaterErrorKind::Metadata, "{error:?}");
        }
    }

    #[test]
    fn a_bad_signature_classifies_as_signature_and_not_as_a_network_blip() {
        // The distinction that matters most: "retry later" versus "do not
        // install this, the release is wrong".
        let error = PluginError::SignatureUtf8("not base64".to_string());
        assert_eq!(classify(&error), UpdaterErrorKind::Signature);
        assert!(from_plugin(&error).starts_with("signature: "));
    }

    #[test]
    fn putting_the_binary_in_place_failing_classifies_as_install() {
        for error in [
            PluginError::TempDirNotFound,
            PluginError::TempDirNotOnSameMountPoint,
            PluginError::BinaryNotFoundInArchive,
            PluginError::InvalidUpdaterFormat,
            PluginError::AuthenticationFailed,
            PluginError::DebInstallFailed,
            PluginError::PackageInstallFailed,
            PluginError::FailedToDetermineExtractPath,
        ] {
            assert_eq!(classify(&error), UpdaterErrorKind::Install, "{error:?}");
        }
    }

    // --- Feature flag -------------------------------------------------------

    #[test]
    fn the_flag_gates_the_commands_and_an_unknown_id_reads_as_off() {
        assert_eq!(feature_refusal(Some(true)), None);

        let off = feature_refusal(Some(false)).expect("an off flag must refuse");
        assert!(off.starts_with("disabled: "), "{off}");
        assert!(off.contains(AUTO_UPDATE_FLAG), "{off}");

        let unknown = feature_refusal(None).expect("an unknown flag must refuse");
        assert!(unknown.starts_with("disabled: "), "{unknown}");
        assert!(unknown.contains("does not recognise"), "{unknown}");
        assert_ne!(off, unknown, "the two must be distinguishable");
    }

    // --- Status assembly ----------------------------------------------------

    #[test]
    fn a_capable_install_with_the_flag_on_reports_ready() {
        let status = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::AppImage),
            Some(true),
            None,
        );
        assert_eq!(status.current_version, "1.2.3");
        assert!(status.supported);
        assert!(status.enabled);
        assert_eq!(status.unsupported_reason, None);
        assert_eq!(status.last_check, None);
    }

    #[test]
    fn the_flag_and_the_install_are_reported_independently() {
        // A capable install with the flag off is supported but disabled, and
        // there is nothing to explain: the toggle says it all.
        let off = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::AppImage),
            Some(false),
            None,
        );
        assert!(off.supported);
        assert!(!off.enabled);
        assert_eq!(off.unsupported_reason, None);

        // An incapable install stays incapable with the flag on.
        let deb = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::Deb),
            Some(true),
            None,
        );
        assert!(!deb.supported);
        assert!(deb.enabled);
        assert!(deb.unsupported_reason.unwrap().contains("dpkg"));
    }

    #[test]
    fn a_flag_this_build_does_not_know_is_off_and_says_why() {
        let status = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::AppImage),
            None,
            None,
        );
        assert!(status.supported, "the install itself is fine");
        assert!(!status.enabled);
        assert!(
            status
                .unsupported_reason
                .as_deref()
                .unwrap()
                .contains(AUTO_UPDATE_FLAG),
            "an unrecognised flag must be distinguishable from a flag switched off"
        );
    }

    #[test]
    fn an_install_that_cannot_update_outranks_the_unknown_flag() {
        let status = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::Deb),
            None,
            None,
        );
        let reason = status.unsupported_reason.unwrap();
        assert!(reason.contains("dpkg"), "{reason}");
        assert!(
            !reason.contains("does not recognise"),
            "toggling a flag cannot make dpkg updatable: {reason}"
        );
    }

    #[test]
    fn the_last_check_is_reported_back_so_it_survives_a_reload() {
        // The previous implementation stored this and never read it, so every
        // webview reload showed "never checked".
        let when = DateTime::parse_from_rfc3339("2026-07-27T10:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let status = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::AppImage),
            Some(true),
            Some(when),
        );
        assert_eq!(
            status.last_check.as_deref(),
            Some("2026-07-27T10:30:00+00:00")
        );
    }

    // --- Wire shapes --------------------------------------------------------

    #[test]
    fn progress_is_internally_tagged_with_snake_case_variant_fields() {
        // Note the asymmetry with every other payload here: on an enum,
        // `rename_all` renames the *variants*, not their fields, so `phase` is
        // camel-cased but `content_length` is not. Pinned by a test because it
        // is exactly the kind of thing a reader assumes the other way round.
        // Making the fields camelCase means adding `rename_all_fields`, which
        // would be a wire break for anything already reading `content_length`.
        let started = serde_json::to_value(UpdaterProgress::Started {
            content_length: Some(1024),
        })
        .unwrap();
        assert_eq!(started["phase"], "started");
        assert_eq!(started["content_length"], 1024);

        let chunk = serde_json::to_value(UpdaterProgress::Chunk {
            downloaded: 512,
            content_length: None,
        })
        .unwrap();
        assert_eq!(chunk["phase"], "chunk");
        assert_eq!(chunk["downloaded"], 512);
        assert!(chunk["content_length"].is_null());

        let finished = serde_json::to_value(UpdaterProgress::Finished).unwrap();
        assert_eq!(finished["phase"], "finished");
    }

    #[test]
    fn status_serialises_the_field_names_the_frontend_reads() {
        let value = serde_json::to_value(build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::Deb),
            Some(true),
            None,
        ))
        .unwrap();
        assert_eq!(value["currentVersion"], "1.2.3");
        assert_eq!(value["supported"], false);
        assert_eq!(value["enabled"], true);
        assert!(value["unsupportedReason"].is_string());
        assert!(value["lastCheck"].is_null());
    }

    // --- Manifest date ------------------------------------------------------

    #[test]
    fn the_publication_date_prefers_the_manifest_spelling() {
        // `OffsetDateTime`'s Display is not RFC 3339, so the parsed value is
        // only a fallback for a manifest that omitted the field.
        let raw = serde_json::json!({ "pub_date": "2026-07-01T12:00:00Z" });
        assert_eq!(
            published_at(&raw, Some("2026-07-01 12:00:00.0 +00:00:00".to_string())),
            Some("2026-07-01T12:00:00Z".to_string())
        );

        let bare = serde_json::json!({ "version": "1.0.0" });
        assert_eq!(
            published_at(&bare, Some("fallback".to_string())),
            Some("fallback".to_string())
        );
        assert_eq!(published_at(&bare, None), None);
    }
}
