// SPDX-License-Identifier: MPL-2.0
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
//! **Nothing here restarts the application, and nothing ends it before the
//! shutdown drain.** SlashIt holds running agents and live PTYs; killing the
//! process to swap a binary would discard both without asking. The install
//! ends by emitting [`UpdaterProgress::Finished`] and stopping. The frontend
//! then does what it already does before a quit — count active processes, show
//! the confirmation dialog — and calls [`updater_restart`] once the user
//! agrees.
//!
//! One platform cannot honor that on its own. On Windows the plugin's install
//! step launches the installer and then calls `std::process::exit(0)` itself,
//! so installing from inside the command would end the process with Workers
//! and provider processes still running and the instance's exclusive local
//! ownership dropped without a drain. [`install_timing`] therefore has Windows
//! download and *stage* the installer, and [`install_staged_after_drain`] runs
//! it from the shutdown path after the instance has quiesced.
//!
//! Progress leaves through the [`EventSink`](crate::events::EventSink) rather
//! than an `AppHandle`, so the flow is not tied to a webview existing.

use crate::commands::gui_admission::GuiMutation;
use crate::config::updates::UpdatePreferences;
use crate::config::ConfigWriteError;
use crate::events::EventSinkExt;
use crate::release_channel::{PlainVersion, ReleaseChannel};
use crate::update_feed::{self, Feed, FeedError, Selection};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
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

/// The environment variable that overrides [`AUTO_UPDATE_FLAG`] at startup.
/// Derived from the flag id by `config::features`; a test keeps them in step.
const AUTO_UPDATE_ENV: &str = "SLASHIT_FEATURE_AUTO_UPDATE";

/// The event name every [`UpdaterProgress`] is emitted under.
pub const PROGRESS_EVENT: &str = "updater://progress";

// --- State ------------------------------------------------------------------

/// What the updater remembers between IPC calls.
///
/// Both fields are read back out: `pending` saves a second round trip to the
/// release endpoint when the user accepts an update they were just shown, and
/// `last_check` is returned by [`updater_status`] so the timestamp survives a
/// webview reload rather than being recomputed as "never".
#[derive(Clone)]
pub struct UpdaterState {
    /// The update the last successful check found, if it found one. Carries the
    /// channel and generation it was found under, so an install can refuse it
    /// once the person has switched channels.
    pending: Arc<Mutex<Option<PendingUpdate>>>,
    /// The update channel the person selected. Read at the start of every
    /// check; changed only by [`updater_set_channel`].
    channel: Arc<Mutex<ReleaseChannel>>,
    /// Which checks are current, and which channel generation is in force. See
    /// [`CheckSequence`].
    checks: Arc<CheckSequence>,
    /// The releases listing from the last successful fetch, and the listing it
    /// was fetched from. Reused for [`update_feed::CACHE_TTL`], only for the same
    /// listing.
    feed_cache: Arc<Mutex<Option<(Instant, String, Feed)>>>,
    /// When the last *successful* check completed.
    last_check: Arc<Mutex<Option<DateTime<Utc>>>>,
    /// Held for the whole download-and-install span via [`InstallGuard`], so
    /// a second concurrent invocation is refused rather than starting its own
    /// `download_and_install` against the same target executable. The
    /// frontend already guards its own "Download and install" button, but
    /// that is UI state, not a backend invariant — the command is callable
    /// regardless, and a stalled install followed by a retry is an ordinary
    /// sequence.
    installing: Arc<std::sync::atomic::AtomicBool>,
    /// A downloaded installer that must not run until the instance has
    /// drained. See [`InstallTiming::AfterDrain`].
    staged: Arc<Mutex<Option<StagedInstall>>>,
}

/// A downloaded update waiting for [`install_staged_after_drain`].
struct StagedInstall {
    update: Update,
    bytes: Vec<u8>,
}

/// When the plugin's install step may run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallTiming {
    /// Inside the install command. The step replaces files and returns.
    Now,
    /// Only once the instance has drained. The step ends the process, so
    /// running it earlier would exit with work still live.
    AfterDrain,
}

/// Windows is the platform whose install step terminates the process (it
/// launches the installer, then `std::process::exit(0)`). Elsewhere the step
/// replaces the installation in place and returns, and the application keeps
/// running until the user restarts it.
pub fn install_timing(os: &str) -> InstallTiming {
    if os == "windows" {
        InstallTiming::AfterDrain
    } else {
        InstallTiming::Now
    }
}

impl UpdaterState {
    /// State for a process whose persisted update channel is `channel`.
    pub fn new(channel: ReleaseChannel) -> Self {
        Self {
            pending: Arc::new(Mutex::new(None)),
            channel: Arc::new(Mutex::new(channel)),
            checks: Arc::new(CheckSequence::default()),
            feed_cache: Arc::new(Mutex::new(None)),
            last_check: Arc::new(Mutex::new(None)),
            installing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            staged: Arc::new(Mutex::new(None)),
        }
    }
}

/// An update the last check found, with what it was found under.
struct PendingUpdate {
    update: Update,
    channel: ReleaseChannel,
    /// The channel generation at the check. Stale once the channel changes.
    generation: u64,
}

/// Decides which asynchronous check results may still write updater state.
///
/// A check takes a [`CheckTicket`] when it starts. Its result is stored only if
/// no later check has started since (the newest request wins, so a slow
/// background poll cannot overwrite a manual check), and only if the channel has
/// not changed since (a late answer from the previous channel is dropped). Both
/// the store and the channel change are made while holding the pending-update
/// lock, so the two orderings cannot interleave.
#[derive(Default)]
pub struct CheckSequence {
    /// Bumped on every channel change.
    generation: std::sync::atomic::AtomicU64,
    /// The number of checks started.
    started: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckTicket {
    generation: u64,
    sequence: u64,
}

impl CheckTicket {
    /// The channel generation the check started under.
    pub fn generation(self) -> u64 {
        self.generation
    }
}

impl CheckSequence {
    pub fn begin(&self) -> CheckTicket {
        use std::sync::atomic::Ordering::SeqCst;
        CheckTicket {
            generation: self.generation.load(SeqCst),
            sequence: self.started.fetch_add(1, SeqCst) + 1,
        }
    }

    pub fn is_current(&self, ticket: CheckTicket) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.generation.load(SeqCst) == ticket.generation && self.started.load(SeqCst) == ticket.sequence
    }

    /// The channel generation now in force.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Invalidate every check in flight and every update found before now.
    pub fn supersede(&self) {
        self.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The channel this build was published on, from `.github/release-channels.txt`
/// at build time (`build.rs`). Never the person's selection.
pub fn installed_channel() -> ReleaseChannel {
    ReleaseChannel::parse(env!("SLASHIT_RELEASE_CHANNEL"))
        .expect("build.rs sets SLASHIT_RELEASE_CHANNEL to a known channel")
}

/// Releases [`UpdaterState::installing`] on every exit path, including the
/// several early `?`/`return Err` points in
/// [`updater_download_and_install`] before the download even starts — a
/// manual release at each of those call sites would only need one to be
/// missed for a refusal to become permanent.
struct InstallGuard(Arc<std::sync::atomic::AtomicBool>);

impl InstallGuard {
    /// `None` if another install is already in flight.
    fn try_acquire(flag: &Arc<std::sync::atomic::AtomicBool>) -> Option<Self> {
        flag.compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .ok()
        .map(|_| Self(flag.clone()))
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
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
    /// The update channel the check ran on: `stable` or `prerelease`.
    pub channel: String,
    pub update: Option<UpdateInfo>,
    pub checked_at: String,
    /// Things the person should know that did not stop the check, such as a
    /// newer release that cannot be installed. Shown verbatim.
    pub notices: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterStatus {
    pub current_version: String,
    /// The channel this build was published on: `stable` or `prerelease`.
    pub installed_channel: String,
    /// The channel the person chose for future updates.
    pub update_channel: String,
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
    /// The answer belongs to a check or an update that the channel has since
    /// replaced. Not a failure: the newer answer is the one to show.
    Superseded,
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
            Self::Superseded => "superseded",
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
///
/// The text names the two switches that exist. There is no Settings control
/// for feature flags; the value is read once at startup, from `features.toml`
/// or from the environment variable, which wins over the file.
fn feature_refusal(flag: Option<bool>, flags_file: &std::path::Path) -> Option<String> {
    match flag {
        Some(true) => None,
        Some(false) => Some(err(
            UpdaterErrorKind::Disabled,
            &format!(
                "Automatic updates are turned off. Set {AUTO_UPDATE_FLAG} = true in {}, \
                 or start SlashIt with {AUTO_UPDATE_ENV}=1. Restart SlashIt to apply the change.",
                flags_file.display()
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
    installed: ReleaseChannel,
    selected: ReleaseChannel,
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
        installed_channel: installed.as_str().to_string(),
        update_channel: selected.as_str().to_string(),
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
    let selected = *state.updater.channel.lock().await;

    Ok(build_status(
        // The same source the plugin compares against, so status and check can
        // never disagree about which version is running.
        app.package_info().version.to_string(),
        &probe_install(&app),
        flag,
        last_check,
        installed_channel(),
        selected,
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
    _gui: GuiMutation,
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<UpdateCheckResult, String> {
    let flag = state.features.read().await.get(AUTO_UPDATE_FLAG);
    if let Some(refusal) = feature_refusal(flag, &state.paths.feature_flags_file()) {
        return Err(refusal);
    }

    let current_version = app.package_info().version.to_string();
    let channel = *state.updater.channel.lock().await;
    let ticket = state.updater.checks.begin();
    let found = find_update(&app, &state, channel, &current_version).await?;

    // The answer is stored only if no later check started and the channel did
    // not change while this one was in flight. The pending lock makes the test
    // and the store one step, so a channel change cannot slip between them.
    let checked_at = Utc::now();
    let info = {
        let mut pending = state.updater.pending.lock().await;
        if !state.updater.checks.is_current(ticket) {
            return Err(superseded_check());
        }
        // Recorded only on success. A timestamp that says "checked a minute
        // ago" after a failed check would read as "you are up to date".
        *state.updater.last_check.lock().await = Some(checked_at);
        match found.update {
            Some(update) => {
                let info = update_to_info(&update, &current_version);
                *pending = Some(PendingUpdate {
                    update,
                    channel,
                    generation: ticket.generation(),
                });
                Some(info)
            }
            None => {
                // Clear it: a stale pending update would let the install command
                // apply a version the user was never shown.
                *pending = None;
                None
            }
        }
    };

    Ok(UpdateCheckResult {
        current_version,
        channel: channel.as_str().to_string(),
        update: info,
        checked_at: checked_at.to_rfc3339(),
        notices: found.notices,
    })
}

/// What one discovery run found.
struct Found {
    update: Option<Update>,
    notices: Vec<String>,
}

fn superseded_check() -> String {
    err(
        UpdaterErrorKind::Superseded,
        "The update channel changed while SlashIt was checking. Check for updates again.",
    )
}

/// Find the update the selected channel offers, if any.
///
/// The releases listing picks the release by version and channel. The plugin
/// then reads that release's own manifest, verifies nothing yet (signatures are
/// checked when the payload is downloaded), and reports whether the manifest is
/// newer than the running build. The manifest must name the release's version.
async fn find_update(
    app: &AppHandle,
    state: &tauri::State<'_, crate::AppState>,
    channel: ReleaseChannel,
    current_version: &str,
) -> Result<Found, String> {
    let current = PlainVersion::parse(current_version).ok_or_else(|| {
        err(
            UpdaterErrorKind::Internal,
            &format!("the running version {current_version} is not a plain MAJOR.MINOR.PATCH"),
        )
    })?;
    let endpoints = update_feed::endpoints()
        .map_err(|detail| err(UpdaterErrorKind::NotConfigured, &detail))?;
    let feed = cached_feed(state, &endpoints).await?;
    let Selection { chosen, notices } = update_feed::select(&feed, channel, current);
    let Some(chosen) = chosen else {
        return Ok(Found { update: None, notices });
    };

    let manifest = update_feed::manifest_url(&endpoints.download_base, &chosen.tag);
    let endpoint = tauri::Url::parse(&manifest).map_err(|error| {
        err(UpdaterErrorKind::NotConfigured, &format!("the release manifest URL is invalid: {error}"))
    })?;
    let updater = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| from_plugin(&e))?
        .timeout(MANIFEST_TIMEOUT);
    // The plugin's client makes the manifest request and the payload download.
    // It is HTTPS only, with a bounded redirect chain, so an HTTP redirect is
    // refused on both. The plugin applies no transport policy of its own.
    let updater = updater.configure_client(update_feed::plugin_client_policy(&endpoints));
    let updater = updater.build().map_err(|e| from_plugin(&e))?;
    let update = match updater.check().await.map_err(|e| from_plugin(&e))? {
        Some(update) => update,
        // The release is newer than this build by its tag, so a manifest the
        // plugin will not offer contradicts the release. That is a broken
        // release, not an up-to-date build, and it must not read as one.
        None => {
            return Err(err(
                UpdaterErrorKind::Metadata,
                &format!(
                    "the release {} publishes a manifest that does not offer a version newer than {current_version}",
                    chosen.tag
                ),
            ))
        }
    };
    // The plugin does not check the payload location the manifest names. This
    // runs before any download, so a refused location is never requested.
    update_feed::check_payload_location(&endpoints, update.download_url.as_str())
        .map_err(|detail| err(UpdaterErrorKind::Metadata, &detail))?;
    update_feed::check_manifest_version(&chosen, &update.version)
        .map_err(|detail| err(UpdaterErrorKind::Metadata, &detail))?;
    Ok(Found {
        update: Some(update),
        notices,
    })
}

/// How long one manifest request may take before it is reported unreachable.
const MANIFEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The releases listing, from the cache when it is fresh enough.
///
/// Held across the request on purpose: concurrent checks wait for one listing
/// rather than each spending a request of the unauthenticated quota.
async fn cached_feed(
    state: &tauri::State<'_, crate::AppState>,
    endpoints: &update_feed::Endpoints,
) -> Result<Feed, String> {
    let mut cache = state.updater.feed_cache.lock().await;
    if let Some((fetched, cached_listing, feed)) = cache.as_ref() {
        if *cached_listing == endpoints.listing && fetched.elapsed() < update_feed::CACHE_TTL {
            return Ok(feed.clone());
        }
    }
    let client = update_feed::client(endpoints).map_err(|error| feed_error(&error))?;
    let feed = update_feed::fetch_feed(&client, &endpoints.listing)
        .await
        .map_err(|error| feed_error(&error))?;
    *cache = Some((Instant::now(), endpoints.listing.clone(), feed.clone()));
    Ok(feed)
}

/// Turn a listing failure into its class. None of these means "up to date".
fn feed_error(error: &FeedError) -> String {
    match error {
        FeedError::Unreachable(detail) => err(
            UpdaterErrorKind::Network,
            &format!("the release list could not be reached: {detail}"),
        ),
        FeedError::RateLimited => err(
            UpdaterErrorKind::Network,
            "GitHub is limiting requests for the release list. Try again later.",
        ),
        FeedError::Status(404) => err(
            UpdaterErrorKind::Metadata,
            "GitHub has no release list for this repository (HTTP 404).",
        ),
        FeedError::Status(status) => err(
            UpdaterErrorKind::Network,
            &format!("GitHub answered HTTP {status} for the release list, so the check did not complete."),
        ),
        FeedError::Malformed(detail) => err(
            UpdaterErrorKind::Metadata,
            &format!("the release list could not be read: {detail}"),
        ),
    }
}

/// Change the channel future checks use. Persisted before it takes effect.
///
/// Refused while an update is being installed or is staged for the restart,
/// since either one was found under the channel being replaced. On success every
/// pending update and every check in flight is invalidated, so nothing found
/// under the old channel can be installed or displayed afterwards. The installed
/// version is not touched; the caller re-checks the new channel.
#[tauri::command]
pub async fn updater_set_channel(
    _gui: GuiMutation,
    state: tauri::State<'_, crate::AppState>,
    channel: String,
) -> Result<(), String> {
    let channel = ReleaseChannel::parse(&channel).ok_or_else(|| {
        err(UpdaterErrorKind::NotConfigured, &format!("{channel:?} is not an update channel"))
    })?;

    let mut pending = state.updater.pending.lock().await;
    if state.updater.installing.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(err(
            UpdaterErrorKind::Install,
            "An update is being installed. Change the channel after it finishes.",
        ));
    }
    if state.updater.staged.lock().await.is_some() {
        return Err(err(
            UpdaterErrorKind::Install,
            "An update is ready to install. Restart SlashIt before changing the channel.",
        ));
    }
    let mut selected = state.updater.channel.lock().await;
    if *selected == channel {
        return Ok(());
    }

    let saved = UpdatePreferences { channel }.save(&state.paths);
    match saved {
        Ok(()) => {}
        // The file already says the new channel. Memory must follow the file,
        // or the next check would run on a channel the file no longer holds.
        Err(ConfigWriteError::PublishedNotDurable(error)) => {
            adopt_channel(&mut selected, &mut pending, &state.updater.checks, channel);
            return Err(err(
                UpdaterErrorKind::Internal,
                &format!("the update channel is now {}, but the change may not survive a crash: {error:#}", channel.as_str()),
            ));
        }
        Err(error) => {
            return Err(err(
                UpdaterErrorKind::Internal,
                &format!("the update channel could not be saved, so it is unchanged: {error:#}"),
            ));
        }
    }

    adopt_channel(&mut selected, &mut pending, &state.updater.checks, channel);
    Ok(())
}

/// Makes a channel the file now holds the effective one in memory. Every check
/// in flight and every update found under the old channel is invalidated. Used
/// for both the durable and the published-but-not-durable outcomes, because in
/// the second the file already says the new channel.
fn adopt_channel<T>(
    selected: &mut ReleaseChannel,
    pending: &mut Option<T>,
    checks: &CheckSequence,
    channel: ReleaseChannel,
) {
    *selected = channel;
    checks.supersede();
    *pending = None;
}

/// Refuses an install whose offered release is not the one the person accepted.
/// The install applies the version it finds, so a channel change or a newer
/// check between display and click must not swap in a different release.
fn accepted_version_refusal(offered: &str, accepted: &str) -> Option<String> {
    (offered != accepted).then(|| {
        err(
            UpdaterErrorKind::Superseded,
            &format!(
                "SlashIt would install {offered}, not the {accepted} you accepted. Check for updates again."
            ),
        )
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
    _gui: GuiMutation,
    app: AppHandle,
    state: tauri::State<'_, crate::AppState>,
    accepted_version: String,
) -> Result<(), String> {
    let flag = state.features.read().await.get(AUTO_UPDATE_FLAG);
    if let Some(refusal) = feature_refusal(flag, &state.paths.feature_flags_file()) {
        return Err(refusal);
    }

    let _install_guard = InstallGuard::try_acquire(&state.updater.installing).ok_or_else(|| {
        err(
            UpdaterErrorKind::Install,
            "An update is already being installed. Wait for it to finish.",
        )
    })?;

    // Before a single byte moves. On an unpackaged Linux build the plugin's
    // fallback rewrites whatever `current_exe()` resolves to, which in a
    // development tree is the developer's own build output.
    let probe = probe_install(&app);
    if let Some(refusal) = capability_refusal(&probe) {
        return Err(refusal);
    }

    // The channel and the pending update are read under the pending lock, which
    // a channel change holds while it checks for an install in progress and
    // writes the new channel. So the channel read here is the one this install
    // runs under, whichever of the two started first.
    let (channel, cached, generation) = {
        let mut pending = state.updater.pending.lock().await;
        let channel = *state.updater.channel.lock().await;
        (channel, pending.take(), state.updater.checks.generation())
    };
    let pending = match cached {
        Some(pending) => {
            // An update found under a channel the person has since left is not
            // the one the current channel offers. Refuse it, do not install it.
            if pending.channel != channel || pending.generation != generation {
                return Err(superseded_check());
            }
            pending
        }
        None => {
            let version = app.package_info().version.to_string();
            let found = find_update(&app, &state, channel, &version).await?;
            let update = found.update.ok_or_else(|| {
                // Say why, when the listing had a reason: a newer release that
                // has no manifest is otherwise indistinguishable from none.
                let mut message = "SlashIt is already on the latest published version.".to_string();
                for notice in &found.notices {
                    message.push(' ');
                    message.push_str(notice);
                }
                err(UpdaterErrorKind::NoUpdate, &message)
            })?;
            PendingUpdate {
                update,
                channel,
                generation,
            }
        }
    };
    // Checked before a byte is downloaded. The release on screen is the one the
    // person accepted; a different release is refused, never installed instead.
    if let Some(refusal) = accepted_version_refusal(&pending.update.version, &accepted_version) {
        return Err(refusal);
    }
    let update = pending.update.clone();

    let events = state.events();
    events.emit(
        PROGRESS_EVENT,
        &UpdaterProgress::Started {
            content_length: None,
        },
    );

    let chunk_events = state.events();
    let mut downloaded: u64 = 0;
    let downloaded_bytes = update
        .download(
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
    let outcome = match downloaded_bytes {
        Ok(bytes) => match install_timing(std::env::consts::OS) {
            InstallTiming::Now => update.install(&bytes),
            InstallTiming::AfterDrain => {
                // Kept for the restart, which drains first.
                *state.updater.staged.lock().await = Some(StagedInstall {
                    update: update.clone(),
                    bytes,
                });
                Ok(())
            }
        },
        Err(e) => Err(e),
    };

    match outcome {
        Ok(()) => {
            events.emit(PROGRESS_EVENT, &UpdaterProgress::Finished);
            Ok(())
        }
        Err(e) => {
            // Put it back. The update is still the one the last check found,
            // and a dropped connection should not force the user to check
            // again before retrying. A check that ran during the install may
            // already have stored an answer; that newer answer wins.
            let mut slot = state.updater.pending.lock().await;
            if slot.is_none() {
                *slot = Some(pending);
            }
            Err(from_plugin(&e))
        }
    }
}

/// Run the installer that [`updater_download_and_install`] staged, if any.
///
/// Called only from the shutdown path, after the instance has fenced new work
/// and reaped every execution owner, and while it still holds its local
/// ownership. On Windows a successful install does not return: the plugin
/// launches the installer and exits the process. An `Err` means the installer
/// did not start, and the caller continues the ordinary restart.
pub async fn install_staged_after_drain(app: &AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let state: tauri::State<'_, crate::AppState> = app.state();
    let staged = state.updater.staged.lock().await.take();
    match staged {
        None => Ok(()),
        Some(staged) => staged
            .update
            .install(&staged.bytes)
            .map_err(|e| from_plugin(&e)),
    }
}

/// Restart into the installed version.
///
/// On Windows this is also what runs the installer, once the drain is done.
///
/// Unconditional by design, and therefore only ever called by a frontend that
/// has already asked. Deliberately not gated on the feature flag: once a new
/// binary is on disk, refusing to restart would leave the application running
/// code that no longer matches its own installation.
///
/// Goes through the same ownership-preserving shutdown as quitting: a restart
/// that exec'd the new binary directly would drop this instance's exclusive
/// local ownership while its agents were still running. The restart happens
/// after the drain, in `run()`'s supervising task.
#[tauri::command]
pub fn updater_restart(app: AppHandle, restart: tauri::State<'_, crate::RestartRequested>) {
    restart.0.store(true, std::sync::atomic::Ordering::SeqCst);
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri_plugin_updater::Error as PluginError;

    #[test]
    fn only_the_newest_check_may_store_its_answer() {
        let checks = CheckSequence::default();
        let background = checks.begin();
        assert!(checks.is_current(background));
        let manual = checks.begin();
        assert!(!checks.is_current(background), "a later check makes the earlier answer stale");
        assert!(checks.is_current(manual));
    }

    #[test]
    fn a_channel_change_makes_every_check_in_flight_stale() {
        let checks = CheckSequence::default();
        let ticket = checks.begin();
        let before = ticket.generation();
        checks.supersede();
        assert!(!checks.is_current(ticket), "a late answer from the old channel must not be stored");
        assert_eq!(checks.generation(), before + 1);
        let fresh = checks.begin();
        assert!(checks.is_current(fresh));
        assert_eq!(fresh.generation(), before + 1);
    }

    #[test]
    fn a_channel_saved_but_not_durable_takes_effect_and_drops_the_old_answer() {
        // The file already says the new channel, so memory must follow it and
        // the update found under the old channel must not survive.
        let checks = CheckSequence::default();
        let mut selected = ReleaseChannel::Stable;
        let mut pending = Some("found under stable");
        let ticket = checks.begin();

        adopt_channel(&mut selected, &mut pending, &checks, ReleaseChannel::Prerelease);

        assert_eq!(selected, ReleaseChannel::Prerelease);
        assert!(pending.is_none(), "an update found under the old channel must be dropped");
        assert!(!checks.is_current(ticket), "a check started under the old channel must not store");
    }

    #[test]
    fn a_successful_channel_change_also_drops_the_old_answer() {
        let checks = CheckSequence::default();
        let mut selected = ReleaseChannel::Prerelease;
        let mut pending = Some("found under prerelease");
        let ticket = checks.begin();

        adopt_channel(&mut selected, &mut pending, &checks, ReleaseChannel::Stable);

        assert_eq!(selected, ReleaseChannel::Stable);
        assert!(pending.is_none());
        assert!(!checks.is_current(ticket));
    }

    #[test]
    fn an_install_of_a_different_release_than_accepted_is_refused() {
        assert!(accepted_version_refusal("0.0.2", "0.0.2").is_none());
        let refusal = accepted_version_refusal("0.0.3", "0.0.2").expect("a mismatch must be refused");
        assert!(refusal.starts_with("superseded: "), "{refusal}");
        assert!(refusal.contains("0.0.3") && refusal.contains("0.0.2"), "{refusal}");
    }

    #[test]
    fn a_listing_failure_is_never_reported_as_up_to_date() {
        let classes = [
            feed_error(&FeedError::Unreachable("dns".into())),
            feed_error(&FeedError::RateLimited),
            feed_error(&FeedError::Status(503)),
            feed_error(&FeedError::Status(403)),
            feed_error(&FeedError::Malformed("not json".into())),
            feed_error(&FeedError::Status(404)),
        ];
        for message in &classes {
            assert!(!message.starts_with("no-update"), "{message}");
        }
        assert!(classes[0].starts_with("network: "));
        assert!(classes[1].starts_with("network: ") && classes[1].contains("limiting"));
        assert!(classes[2].starts_with("network: ") && classes[2].contains("503"));
        assert!(classes[4].starts_with("metadata: "));
        assert!(classes[5].starts_with("metadata: "));
    }

    #[test]
    fn the_build_reports_the_channel_its_version_is_declared_on() {
        let text = include_str!("../../../.github/release-channels.txt");
        let declarations = crate::release_channel::parse_declarations(text).unwrap();
        let version = PlainVersion::parse(env!("CARGO_PKG_VERSION")).unwrap();
        assert_eq!(
            crate::release_channel::channel_for(&declarations, version),
            Some(installed_channel())
        );
    }

    #[test]
    fn the_status_reports_the_installed_build_apart_from_the_selection() {
        let value = serde_json::to_value(build_status(
            "0.0.1".to_string(),
            &probe("linux", InstallKind::AppImage),
            Some(true),
            None,
            ReleaseChannel::Prerelease,
            ReleaseChannel::Stable,
        ))
        .unwrap();
        assert_eq!(value["installedChannel"], "prerelease");
        assert_eq!(value["updateChannel"], "stable");
    }

    #[test]
    fn a_second_install_is_refused_while_the_first_holds_the_guard() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let first = InstallGuard::try_acquire(&flag).expect("first acquire must succeed");
        assert!(
            InstallGuard::try_acquire(&flag).is_none(),
            "a second acquire must be refused while the first guard is held"
        );

        drop(first);
        assert!(
            InstallGuard::try_acquire(&flag).is_some(),
            "dropping the guard must release the flag for the next caller"
        );
    }

    /// The plugin's Windows install step launches the installer and then
    /// exits the process. It must never run inside the install command, where
    /// Workers and provider processes may still be live; only the drained
    /// shutdown path may run it. Every other platform replaces files in place
    /// and returns, so it keeps installing inside the command.
    #[test]
    fn only_windows_defers_the_installer_until_the_instance_has_drained() {
        assert_eq!(install_timing("windows"), InstallTiming::AfterDrain);
        for os in ["linux", "macos"] {
            assert_eq!(install_timing(os), InstallTiming::Now, "{os}");
        }
    }

    /// The plugin's one-call download-and-install would run the Windows
    /// installer, and so end the process, wherever it is called from. The only
    /// install calls allowed are the two in this module, which are ordered
    /// around the drain.
    #[test]
    fn no_code_calls_the_plugins_combined_download_and_install() {
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>, needle: &str) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, hits, needle);
                } else if path.extension().is_some_and(|e| e == "rs")
                    && std::fs::read_to_string(&path).unwrap().contains(needle)
                {
                    hits.push(path.display().to_string());
                }
            }
        }
        let needle = [".download_and", "_install("].concat();
        let mut hits = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut hits, &needle);
        assert!(hits.is_empty(), "the combined call bypasses the drain on Windows: {hits:?}");
    }

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
        let file = std::path::Path::new("/cfg/slashit-app/features.toml");
        assert_eq!(feature_refusal(Some(true), file), None);

        let off = feature_refusal(Some(false), file).expect("an off flag must refuse");
        assert!(off.starts_with("disabled: "), "{off}");
        assert!(off.contains(AUTO_UPDATE_FLAG), "{off}");

        let unknown = feature_refusal(None, file).expect("an unknown flag must refuse");
        assert!(unknown.starts_with("disabled: "), "{unknown}");
        assert!(unknown.contains("does not recognise"), "{unknown}");
        assert_ne!(off, unknown, "the two must be distinguishable");
    }

    #[test]
    fn the_off_refusal_names_the_file_and_the_switches_that_exist() {
        let file = std::path::Path::new("/cfg/slashit-app/features.toml");
        let off = feature_refusal(Some(false), file).expect("an off flag must refuse");

        assert!(off.contains("/cfg/slashit-app/features.toml"), "{off}");
        assert!(off.contains("auto_update = true"), "{off}");
        assert!(off.contains("SLASHIT_FEATURE_AUTO_UPDATE=1"), "{off}");
        assert!(off.contains("Restart SlashIt"), "{off}");
        // There is no Settings control for flags; the text must not send the
        // user looking for one.
        assert!(!off.contains("Settings"), "{off}");
    }

    #[test]
    fn the_env_override_name_is_the_one_the_registry_derives() {
        assert_eq!(
            AUTO_UPDATE_ENV,
            format!(
                "{}{}",
                crate::config::features::ENV_PREFIX,
                AUTO_UPDATE_FLAG.to_ascii_uppercase()
            )
        );
    }

    // --- Status assembly ----------------------------------------------------

    #[test]
    fn a_capable_install_with_the_flag_on_reports_ready() {
        let status = build_status(
            "1.2.3".to_string(),
            &probe("linux", InstallKind::AppImage),
            Some(true),
            None,
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
            ReleaseChannel::Stable,
            ReleaseChannel::Stable,
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
