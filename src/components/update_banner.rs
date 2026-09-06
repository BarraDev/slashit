//! Update UX: the shared updater state, the floating notice, and the restart
//! confirmation.
//!
//! Both consumers — this notice and Settings > Updates — derive everything they
//! show from one `UpdaterContext`. That is deliberate. The earlier version kept
//! a private status string inside Settings, so a failed check could keep
//! claiming failure long after a later successful check had disproved it.
//!
//! The other rule this module exists to enforce: no state is ever rendered as
//! more finished than it is. A download that stopped reporting is "stopped
//! reporting", not "installing". An install that succeeded is "installed, not
//! yet running" until the user agrees to restart — because restarting kills
//! every terminal and agent the user has open.

use crate::services::tray_service::get_active_process_count;
use crate::services::updater_service::{
    clear_skipped_version, format_bytes, get_skipped_version, set_skipped_version,
    subscribe_updater_progress, updater_check, updater_download_and_install, updater_restart,
    updater_status, UpdateInfo, UpdaterFailure, UpdaterProgress, UpdaterStage, UpdaterStatus,
};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;

/// Delay before the first background check so it never competes with startup.
const INITIAL_DELAY_MS: u32 = 10_000;
const POLL_INTERVAL_MS: u32 = 6 * 60 * 60 * 1000;
/// A transfer that emits no progress for this long is reported as stalled and
/// gains an escape hatch.
const STALL_TIMEOUT_MS: u32 = 90_000;

// Distinct glyphs per state. "An update exists", "it is transferring", "it is
// being written to disk" and "restart to run it" are four different facts and
// previously shared one generic plus sign, which made them indistinguishable.
const ICON_AVAILABLE: &str = "M12 3v11m0 0l-4-4m4 4l4-4M4 15v3a2 2 0 002 2h12a2 2 0 002-2v-3";
const ICON_TRANSFER: &str = "M12 3a9 9 0 109 9";
const ICON_INSTALLING: &str =
    "M20 7.5l-8-4.5-8 4.5m16 0l-8 4.5m8-4.5v9l-8 4.5m0-9L4 7.5m8 4.5v9M4 7.5v9";
const ICON_RESTART: &str = "M12 3v9M18.36 6.64a9 9 0 11-12.72 0";
const ICON_ALERT: &str =
    "M12 9v4m0 4h.01M10.29 3.86L1.82 18a2 2 0 001.71 3h16.94a2 2 0 001.71-3L13.71 3.86a2 2 0 00-3.42 0z";

const CARD_BASE: &str = "pointer-events-auto rounded-xl border shadow-2xl backdrop-blur-sm bg-[#0B0B0F]/95 p-4 text-sm text-white/90";

/// What restarting would terminate, as far as the app could determine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActiveProcesses {
    pub pty: usize,
    pub agents: usize,
    /// False when the count could not be read. A failed count is not evidence
    /// that nothing is running, so the confirmation is still shown.
    pub known: bool,
}

/// The one operation the updater is performing right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdaterActivity {
    Idle,
    Checking,
    Downloading {
        downloaded: u64,
        total: Option<u64>,
    },
    Installing,
    /// Written to disk but not running. The backend no longer restarts by
    /// itself, so this state is real and the user has to leave it deliberately.
    RestartRequired,
    Restarting,
}

impl UpdaterActivity {
    pub fn busy(&self) -> bool {
        !matches!(self, UpdaterActivity::Idle)
    }
}

/// Every signal the update UI reads. `Copy`, so it can be handed to event
/// handlers and background tasks without cloning.
#[derive(Clone, Copy)]
pub struct UpdaterContext {
    /// `None` means capability was never established — treated as "cannot
    /// install", never as "can".
    pub status: RwSignal<Option<UpdaterStatus>>,
    pub available: RwSignal<Option<UpdateInfo>>,
    pub last_check: RwSignal<Option<String>>,
    /// Separates "no update found" from "never looked".
    pub checked_once: RwSignal<bool>,
    pub activity: RwSignal<UpdaterActivity>,
    pub failure: RwSignal<Option<UpdaterFailure>>,
    pub failure_dismissed: RwSignal<bool>,
    /// Session-scoped "Later". "Skip this version" is the persistent form and
    /// lives in localStorage instead.
    pub dismissed_version: RwSignal<Option<String>>,
    pub restart_deferred: RwSignal<bool>,
    /// `Some` renders the restart confirmation, carrying what it would kill.
    pub restart_prompt: RwSignal<Option<ActiveProcesses>>,
    /// True while a check the user explicitly asked for is running, so the
    /// silent background poll does not flash a notice every six hours.
    pub check_user_initiated: RwSignal<bool>,
    /// Bumped by every progress event; the stall watchdog compares it against
    /// the value it captured to tell "slow" from "stuck".
    pub progress_tick: RwSignal<u32>,
    pub stalled: RwSignal<bool>,
    /// Bumped by every `start_install` call. A watchdog captures the value at
    /// the moment it is spawned and only acts if it still matches when its
    /// timer fires — otherwise a failed attempt's watchdog can outlive it and
    /// wrongly mark a later retry stalled before that retry's own timeout.
    pub install_generation: RwSignal<u32>,
}

impl UpdaterContext {
    /// Why in-app updating is unavailable, or `None` when it is available.
    /// `Some` gates every install affordance in the UI.
    pub fn blocked_reason(&self) -> Option<String> {
        match self.status.get() {
            None => Some(
                "SlashIt could not determine whether this build can update itself, so in-app updating is unavailable."
                    .to_string(),
            ),
            Some(s) if !s.supported => Some(s.unsupported_reason.clone().unwrap_or_else(|| {
                "This build does not support in-app updates. Update it the way it was installed."
                    .to_string()
            })),
            Some(s) if !s.enabled => {
                Some("In-app updates are disabled by configuration for this installation.".to_string())
            }
            Some(_) => None,
        }
    }

    /// True only once a check has actually completed and reported nothing newer.
    pub fn up_to_date(&self) -> bool {
        self.checked_once.get()
            && self.available.get().is_none()
            && self.failure.get().is_none()
            && !self.activity.get().busy()
    }

    /// The running version, or `None` when it was never established.
    pub fn current_version(&self) -> Option<String> {
        self.status.get().map(|s| s.current_version)
    }

    /// Whether an available update at `version` should still be surfaced,
    /// given a session "Later" or a persisted "Skip this version". Shared by
    /// the floating notice and Settings > Updates so the two can never
    /// disagree about whether the user has already dealt with a version —
    /// each previously checked this on its own, and only the notice actually
    /// did.
    pub fn update_is_visible(&self, version: &str) -> bool {
        if self.dismissed_version.get().as_deref() == Some(version) {
            return false;
        }
        if get_skipped_version().as_deref() == Some(version) {
            return false;
        }
        true
    }

    fn record_failure(&self, failure: UpdaterFailure) {
        // A new failure un-dismisses the notice: the user acknowledged the
        // previous problem, not this one.
        self.failure_dismissed.set(false);
        self.failure.set(Some(failure));
    }
}

/// Creates the shared updater signals, wires the progress subscription, and
/// starts the background poll. Call once from `App`.
pub fn provide_updater_context() -> UpdaterContext {
    let ctx = UpdaterContext {
        status: RwSignal::new(None),
        available: RwSignal::new(None),
        last_check: RwSignal::new(None),
        checked_once: RwSignal::new(false),
        activity: RwSignal::new(UpdaterActivity::Idle),
        failure: RwSignal::new(None),
        failure_dismissed: RwSignal::new(false),
        dismissed_version: RwSignal::new(None),
        restart_deferred: RwSignal::new(false),
        restart_prompt: RwSignal::new(None),
        check_user_initiated: RwSignal::new(false),
        progress_tick: RwSignal::new(0),
        stalled: RwSignal::new(false),
        install_generation: RwSignal::new(0),
    };
    provide_context(ctx);

    // Subscribed here rather than in the notice component: the notice can be
    // unmounted or dismissed mid-download, and a dropped subscription would
    // strand the progress bar at zero for the rest of the transfer.
    subscribe_updater_progress(move |progress| {
        ctx.progress_tick.update(|t| *t = t.wrapping_add(1));
        ctx.stalled.set(false);
        match progress {
            UpdaterProgress::Started { content_length } => {
                ctx.activity.set(UpdaterActivity::Downloading {
                    downloaded: 0,
                    total: content_length,
                });
            }
            UpdaterProgress::Chunk {
                downloaded,
                content_length,
            } => {
                ctx.activity.set(UpdaterActivity::Downloading {
                    downloaded,
                    total: content_length,
                });
            }
            UpdaterProgress::Finished => {
                // Only advance from a download. The install command's own `Ok`
                // is what proves the install finished, and it can resolve
                // before this event arrives — regressing to "Installing" then
                // would undo a truthful "restart required".
                if matches!(
                    ctx.activity.get_untracked(),
                    UpdaterActivity::Downloading { .. }
                ) {
                    ctx.activity.set(UpdaterActivity::Installing);
                }
            }
        }
    });

    Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true;
        }
        spawn_local(async move {
            // Capability first. A build that cannot update is not polled at
            // all, and never gets an install button anywhere in the UI.
            match updater_status().await {
                Ok(status) => {
                    if status.last_check.is_some() {
                        ctx.last_check.set(status.last_check.clone());
                    }
                    let can_update = status.supported && status.enabled;
                    ctx.status.set(Some(status));
                    if !can_update {
                        return;
                    }
                }
                Err(e) => {
                    ctx.record_failure(UpdaterFailure::new(UpdaterStage::Status, e));
                    return;
                }
            }
            TimeoutFuture::new(INITIAL_DELAY_MS).await;
            loop {
                run_check(ctx).await;
                TimeoutFuture::new(POLL_INTERVAL_MS).await;
            }
        });
        true
    });

    ctx
}

async fn run_check(ctx: UpdaterContext) {
    if ctx.activity.get_untracked().busy() {
        return;
    }
    // Clearing the previous error at the start of the attempt is what makes
    // error state self-clearing: a failure can never outlive the check that
    // superseded it.
    ctx.failure.set(None);
    ctx.activity.set(UpdaterActivity::Checking);

    match updater_check().await {
        Ok(result) => {
            ctx.last_check.set(Some(result.checked_at));
            // Keep the displayed version honest even if `updater_status` was
            // read before some other part of the app changed it.
            ctx.status.update(|s| {
                if let Some(status) = s {
                    status.current_version = result.current_version;
                }
            });
            ctx.available.set(result.update);
            ctx.checked_once.set(true);
            ctx.activity.set(UpdaterActivity::Idle);
        }
        Err(e) => {
            ctx.activity.set(UpdaterActivity::Idle);
            ctx.record_failure(UpdaterFailure::new(UpdaterStage::Check, e));
        }
    }
}

/// A check the user asked for. Unlike the background poll it un-skips whatever
/// it finds — asking for a check is asking to see the answer — and it is
/// visible while it runs.
pub fn check_now(ctx: UpdaterContext) {
    if ctx.activity.get_untracked().busy() {
        return;
    }
    ctx.check_user_initiated.set(true);
    spawn_local(async move {
        run_check(ctx).await;
        if let Some(update) = ctx.available.get_untracked() {
            if get_skipped_version().as_deref() == Some(update.version.as_str()) {
                clear_skipped_version();
            }
            ctx.dismissed_version.set(None);
        }
        ctx.check_user_initiated.set(false);
    });
}

pub fn start_install(ctx: UpdaterContext) {
    if ctx.activity.get_untracked().busy() {
        return;
    }
    // Last line of defence. The UI already hides the button in this case; if it
    // is ever reachable, refusing loudly beats a no-op that looks like success.
    if let Some(reason) = ctx.blocked_reason() {
        ctx.record_failure(UpdaterFailure::new(
            UpdaterStage::Install,
            format!("unsupported: {reason}"),
        ));
        return;
    }

    ctx.failure.set(None);
    ctx.stalled.set(false);
    // Starts indeterminate: the content length is only known once the backend's
    // first progress event arrives, and may never be reported at all.
    ctx.activity.set(UpdaterActivity::Downloading {
        downloaded: 0,
        total: None,
    });
    let generation = ctx.install_generation.get_untracked().wrapping_add(1);
    ctx.install_generation.set(generation);
    watch_for_stall(ctx, generation);

    spawn_local(async move {
        let outcome = updater_download_and_install().await;
        // The backend serialises installs, so `abandon_stalled` followed by a
        // retry usually gets an immediate "already installing" error rather
        // than a second real download — but the guard is released the instant
        // the *backend* call returns, which can be before *this* future's own
        // await resolves. In that window a retry can start a genuine second
        // install, and this completion must not apply if a later attempt has
        // since started: it would overwrite that attempt's own Downloading or
        // Installing state with a stale Idle/RestartRequired/failure.
        if ctx.install_generation.get_untracked() != generation {
            return;
        }
        match outcome {
            Ok(()) => {
                ctx.stalled.set(false);
                ctx.restart_deferred.set(false);
                ctx.activity.set(UpdaterActivity::RestartRequired);
            }
            Err(e) => {
                ctx.activity.set(UpdaterActivity::Idle);
                ctx.record_failure(UpdaterFailure::new(UpdaterStage::Install, e));
            }
        }
    });
}

/// Nothing here can cancel the backend. The watchdog exists so a transfer that
/// stops reporting cannot pin the UI in a state with no exit: after the timeout
/// the user gets a way out that says plainly the work may still be running.
///
/// `generation` is the value `install_generation` held when this watchdog was
/// spawned. A failed attempt leaves this loop running; if the user retries
/// before the timeout fires, `install_generation` moves on and this check
/// stops the stale watchdog from marking the new attempt stalled before its
/// own timeout has actually elapsed.
fn watch_for_stall(ctx: UpdaterContext, generation: u32) {
    spawn_local(async move {
        loop {
            let before = ctx.progress_tick.get_untracked();
            TimeoutFuture::new(STALL_TIMEOUT_MS).await;
            if ctx.install_generation.get_untracked() != generation {
                return;
            }
            if !matches!(
                ctx.activity.get_untracked(),
                UpdaterActivity::Downloading { .. } | UpdaterActivity::Installing
            ) {
                return;
            }
            if ctx.progress_tick.get_untracked() == before {
                ctx.stalled.set(true);
                return;
            }
        }
    });
}

/// Stop showing a stalled transfer without claiming it was cancelled, because
/// it was not.
pub fn abandon_stalled(ctx: UpdaterContext) {
    ctx.stalled.set(false);
    ctx.activity.set(UpdaterActivity::Idle);
    ctx.record_failure(UpdaterFailure::new(
        UpdaterStage::Install,
        "stalled: the update stopped reporting progress and was hidden; it may still be running",
    ));
}

/// Ask to restart. Restarting kills every terminal and agent, so it goes
/// through the same confirmation a manual quit gets rather than happening as a
/// side effect of finishing an install.
pub fn request_restart(ctx: UpdaterContext) {
    spawn_local(async move {
        let counts = match get_active_process_count().await {
            Ok(value) => ActiveProcesses {
                pty: count_field(&value, "pty", "pty_count"),
                agents: count_field(&value, "agents", "agent_count"),
                known: true,
            },
            // A failed count is not proof that nothing is running, so confirm.
            Err(e) => {
                leptos::logging::warn!("[updater] could not count active processes: {}", e);
                ActiveProcesses::default()
            }
        };
        if counts.known && counts.pty == 0 && counts.agents == 0 {
            perform_restart(ctx);
        } else {
            ctx.restart_prompt.set(Some(counts));
        }
    });
}

/// `get_active_process_count` returns `{pty, agents}`, while the backend's
/// `quit-requested` event uses `{pty_count, agent_count}`. Accept both rather
/// than silently reporting zero if the command is aligned with the event.
fn count_field(value: &serde_json::Value, primary: &str, fallback: &str) -> usize {
    value
        .get(primary)
        .or_else(|| value.get(fallback))
        .and_then(|n| n.as_u64())
        .unwrap_or(0) as usize
}

fn perform_restart(ctx: UpdaterContext) {
    ctx.restart_prompt.set(None);
    ctx.activity.set(UpdaterActivity::Restarting);
    spawn_local(async move {
        // On success the process is replaced and this never resolves, so only
        // the error arm can run.
        if let Err(e) = updater_restart().await {
            ctx.activity.set(UpdaterActivity::RestartRequired);
            ctx.record_failure(UpdaterFailure::new(UpdaterStage::Restart, e));
        }
    });
}

#[component]
pub fn UpdateBanner() -> impl IntoView {
    let Some(ctx) = use_context::<UpdaterContext>() else {
        // Rendering nothing beats taking the whole window down over a notice.
        leptos::logging::error!("[updater] UpdateBanner mounted without UpdaterContext");
        return ().into_any();
    };

    view! {
        <div class="fixed top-4 right-4 z-[60] flex w-[26rem] max-w-[calc(100vw-2rem)] flex-col gap-2 pointer-events-none">
            {move || activity_card(ctx)}
            {move || failure_card(ctx)}
            {move || available_card(ctx)}
        </div>
        <RestartConfirm />
    }
    .into_any()
}

fn activity_card(ctx: UpdaterContext) -> AnyView {
    match ctx.activity.get() {
        UpdaterActivity::Idle => ().into_any(),
        UpdaterActivity::Checking => {
            if ctx.check_user_initiated.get() {
                checking_card()
            } else {
                ().into_any()
            }
        }
        UpdaterActivity::Downloading { downloaded, total } => {
            downloading_card(ctx, downloaded, total)
        }
        UpdaterActivity::Installing => installing_card(ctx),
        UpdaterActivity::RestartRequired => {
            if ctx.restart_deferred.get() {
                ().into_any()
            } else {
                restart_card(ctx)
            }
        }
        UpdaterActivity::Restarting => restarting_card(),
    }
}

fn checking_card() -> AnyView {
    view! {
        <div class=format!("{CARD_BASE} border-white/10")>
            <div class="flex items-center gap-3">
                <svg class="w-5 h-5 shrink-0 animate-spin text-white/60" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                    <path stroke-linecap="round" d=ICON_TRANSFER />
                </svg>
                <p class="font-medium">"Checking for updates..."</p>
            </div>
        </div>
    }
    .into_any()
}

fn downloading_card(ctx: UpdaterContext, downloaded: u64, total: Option<u64>) -> AnyView {
    // Determinate only when the backend actually reported a content length.
    let percent = total
        .filter(|t| *t > 0)
        .map(|t| ((downloaded as f64 / t as f64) * 100.0).clamp(0.0, 100.0));
    let detail = match (percent, total) {
        (Some(p), Some(t)) => format!(
            "{} of {} ({:.0}%)",
            format_bytes(downloaded),
            format_bytes(t),
            p
        ),
        // Never invent a total the backend did not send.
        _ => format!(
            "{} downloaded (total size not reported)",
            format_bytes(downloaded)
        ),
    };
    let stalled = ctx.stalled.get();

    view! {
        <div class=format!("{CARD_BASE} border-blue-500/40")>
            <div class="flex items-center gap-3">
                <svg class="w-5 h-5 shrink-0 animate-spin text-blue-300" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                    <path stroke-linecap="round" d=ICON_TRANSFER />
                </svg>
                <div class="min-w-0 flex-1">
                    <p class="font-medium">"Downloading update"</p>
                    <p class="text-xs text-white/50 mt-0.5">{detail}</p>
                </div>
            </div>
            <div class="mt-3 h-1.5 rounded-full bg-white/10 overflow-hidden">
                {match percent {
                    Some(p) => view! {
                        <div class="h-full rounded-full bg-blue-400 transition-all" style=format!("width: {p:.1}%")></div>
                    }.into_any(),
                    None => view! {
                        <div class="h-full w-1/3 rounded-full bg-blue-400 animate-pulse"></div>
                    }.into_any(),
                }}
            </div>
            {stalled.then(|| view! {
                <div class="mt-3 flex items-center justify-between gap-3">
                    <p class="text-xs text-amber-300">"No progress for a while. It may still be running."</p>
                    <button
                        on:click=move |_| abandon_stalled(ctx)
                        class="shrink-0 px-2.5 py-1 rounded-lg text-xs text-white/70 hover:text-white/90 hover:bg-white/5"
                    >
                        "Hide"
                    </button>
                </div>
            })}
        </div>
    }
    .into_any()
}

fn installing_card(ctx: UpdaterContext) -> AnyView {
    let stalled = ctx.stalled.get();
    view! {
        <div class=format!("{CARD_BASE} border-blue-500/40")>
            <div class="flex items-center gap-3">
                <svg class="w-5 h-5 shrink-0 text-blue-300 animate-pulse" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="1.75">
                    <path stroke-linecap="round" stroke-linejoin="round" d=ICON_INSTALLING />
                </svg>
                <div class="min-w-0 flex-1">
                    <p class="font-medium">"Installing update"</p>
                    <p class="text-xs text-white/50 mt-0.5">"SlashIt will keep running until you restart it."</p>
                </div>
            </div>
            {stalled.then(|| view! {
                <div class="mt-3 flex items-center justify-between gap-3">
                    <p class="text-xs text-amber-300">"This is taking longer than expected."</p>
                    <button
                        on:click=move |_| abandon_stalled(ctx)
                        class="shrink-0 px-2.5 py-1 rounded-lg text-xs text-white/70 hover:text-white/90 hover:bg-white/5"
                    >
                        "Hide"
                    </button>
                </div>
            })}
        </div>
    }
    .into_any()
}

fn restart_card(ctx: UpdaterContext) -> AnyView {
    let version = ctx.available.get().map(|u| u.version);
    let line = match version {
        Some(v) => format!("SlashIt {v} is installed but not running yet."),
        None => "The update is installed but not running yet.".to_string(),
    };
    view! {
        <div class=format!("{CARD_BASE} border-emerald-500/40")>
            <div class="flex items-center gap-3">
                <svg class="w-5 h-5 shrink-0 text-emerald-300" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                    <path stroke-linecap="round" stroke-linejoin="round" d=ICON_RESTART />
                </svg>
                <div class="min-w-0 flex-1">
                    <p class="font-medium">"Restart to finish"</p>
                    <p class="text-xs text-white/50 mt-0.5">{line}</p>
                </div>
            </div>
            <div class="mt-3 flex items-center justify-end gap-2">
                <button
                    on:click=move |_| ctx.restart_deferred.set(true)
                    class="px-2.5 py-1 rounded-lg text-xs text-white/60 hover:text-white/90 hover:bg-white/5"
                >
                    "Later"
                </button>
                <button
                    on:click=move |_| request_restart(ctx)
                    class="px-3 py-1.5 rounded-lg bg-emerald-500 text-black text-xs font-semibold hover:bg-emerald-400"
                >
                    "Restart now"
                </button>
            </div>
        </div>
    }
    .into_any()
}

fn restarting_card() -> AnyView {
    view! {
        <div class=format!("{CARD_BASE} border-emerald-500/40")>
            <div class="flex items-center gap-3">
                <svg class="w-5 h-5 shrink-0 animate-spin text-emerald-300" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                    <path stroke-linecap="round" d=ICON_TRANSFER />
                </svg>
                <p class="font-medium">"Restarting SlashIt..."</p>
            </div>
        </div>
    }
    .into_any()
}

fn failure_card(ctx: UpdaterContext) -> AnyView {
    if ctx.failure_dismissed.get() {
        return ().into_any();
    }
    // `if let` rather than the earlier `.expect()` inside a `Show` child: the
    // very buttons rendered here can empty this signal, and the child closure
    // re-runs after they do.
    let Some(failure) = ctx.failure.get() else {
        return ().into_any();
    };

    let retry = failure.kind.retryable().then_some(failure.stage);
    let headline = failure.kind.headline();
    let hint = failure.kind.hint();
    let detail = format!("{}: {}", failure.stage.label(), failure.detail);

    view! {
        <div class=format!("{CARD_BASE} border-red-500/40")>
            <div class="flex items-start gap-3">
                <svg class="w-5 h-5 shrink-0 text-red-300 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                    <path stroke-linecap="round" stroke-linejoin="round" d=ICON_ALERT />
                </svg>
                <div class="min-w-0 flex-1">
                    <p class="font-medium text-red-200">{headline}</p>
                    <p class="text-xs text-white/60 mt-1">{hint}</p>
                    <p class="text-[11px] text-white/35 mt-2 font-mono break-words">{detail}</p>
                </div>
            </div>
            <div class="mt-3 flex items-center justify-end gap-2">
                <button
                    on:click=move |_| ctx.failure_dismissed.set(true)
                    class="px-2.5 py-1 rounded-lg text-xs text-white/60 hover:text-white/90 hover:bg-white/5"
                >
                    "Dismiss"
                </button>
                {retry.map(|stage| {
                    let label = match stage {
                        UpdaterStage::Restart => "Restart now",
                        UpdaterStage::Install => "Try again",
                        UpdaterStage::Status | UpdaterStage::Check => "Check again",
                    };
                    view! {
                        <button
                            on:click=move |_| match stage {
                                UpdaterStage::Restart => request_restart(ctx),
                                UpdaterStage::Install => start_install(ctx),
                                UpdaterStage::Status | UpdaterStage::Check => check_now(ctx),
                            }
                            class="px-3 py-1.5 rounded-lg bg-white/10 hover:bg-white/15 text-xs font-semibold text-white/85"
                        >
                            {label}
                        </button>
                    }
                })}
            </div>
        </div>
    }
    .into_any()
}

fn available_card(ctx: UpdaterContext) -> AnyView {
    if ctx.activity.get().busy() {
        return ().into_any();
    }
    let Some(update) = ctx.available.get() else {
        return ().into_any();
    };
    if !ctx.update_is_visible(&update.version) {
        return ().into_any();
    }

    let blocked = ctx.blocked_reason();
    let headline = format!("SlashIt {} is available", update.version);
    let subline = format!("You are running {}.", update.current_version);
    let later_version = update.version.clone();
    let skip_version = update.version.clone();

    view! {
        <div class=format!("{CARD_BASE} border-amber-500/40")>
            <div class="flex items-start gap-3">
                <svg class="w-5 h-5 shrink-0 text-amber-300 mt-0.5" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                    <path stroke-linecap="round" stroke-linejoin="round" d=ICON_AVAILABLE />
                </svg>
                <div class="min-w-0 flex-1">
                    <p class="font-medium">{headline}</p>
                    <p class="text-xs text-white/50 mt-0.5">{subline}</p>
                    {update.date.clone().map(|d| view! {
                        <p class="text-xs text-white/35 mt-0.5">{format!("Released {d}")}</p>
                    })}
                </div>
            </div>
            <div class="mt-3 flex items-center justify-end gap-2">
                <button
                    on:click=move |_| {
                        set_skipped_version(&skip_version);
                        ctx.dismissed_version.set(Some(skip_version.clone()));
                    }
                    class="px-2.5 py-1 rounded-lg text-xs text-white/50 hover:text-white/80 hover:bg-white/5"
                >
                    "Skip this version"
                </button>
                <button
                    on:click=move |_| ctx.dismissed_version.set(Some(later_version.clone()))
                    class="px-2.5 py-1 rounded-lg text-xs text-white/60 hover:text-white/90 hover:bg-white/5"
                >
                    "Later"
                </button>
                // An install button on a build that cannot install is a lie, so
                // the reason replaces the button rather than sitting next to it.
                {match blocked {
                    Some(reason) => view! {
                        <p class="text-xs text-amber-200/80 text-right">{reason}</p>
                    }.into_any(),
                    None => view! {
                        <button
                            on:click=move |_| start_install(ctx)
                            class="px-3 py-1.5 rounded-lg bg-amber-500 text-black text-xs font-semibold hover:bg-amber-400"
                        >
                            "Download and install"
                        </button>
                    }.into_any(),
                }}
            </div>
        </div>
    }
    .into_any()
}

/// Restart confirmation. Mirrors `QuitDialog` on purpose: an update must not be
/// able to destroy running work through a path the user has not seen before.
#[component]
fn RestartConfirm() -> impl IntoView {
    let Some(ctx) = use_context::<UpdaterContext>() else {
        return ().into_any();
    };

    view! {
        {move || {
            let Some(counts) = ctx.restart_prompt.get() else {
                return ().into_any();
            };
            view! {
                <div class="fixed inset-0 z-[100] flex items-center justify-center p-4">
                    <div
                        class="absolute inset-0 bg-black/60 backdrop-blur-sm"
                        on:click=move |_| ctx.restart_prompt.set(None)
                    ></div>
                    <div class="relative w-full max-w-md bg-[#0B0B0F] border border-white/10 rounded-xl shadow-2xl" on:click=move |e| e.stop_propagation()>
                        <div class="p-6">
                            <div class="flex items-center gap-3 mb-4">
                                <svg class="w-6 h-6 text-amber-400" fill="none" viewBox="0 0 24 24" stroke="currentColor" stroke-width="2">
                                    <path stroke-linecap="round" stroke-linejoin="round" d=ICON_ALERT />
                                </svg>
                                <h2 class="text-lg font-semibold text-white/90">"Restart to finish the update"</h2>
                            </div>
                            {if counts.known {
                                view! {
                                    <div>
                                        <p class="text-white/60 text-sm mb-4">"There are still active processes running:"</p>
                                        <div class="space-y-2 mb-6">
                                            <Show when=move || counts.pty != 0>
                                                <div class="flex items-center gap-2 text-sm text-white/70">
                                                    <span class="w-2 h-2 rounded-full bg-green-400"></span>
                                                    {format!("{} terminal session{}", counts.pty, if counts.pty != 1 { "s" } else { "" })}
                                                </div>
                                            </Show>
                                            <Show when=move || counts.agents != 0>
                                                <div class="flex items-center gap-2 text-sm text-white/70">
                                                    <span class="w-2 h-2 rounded-full bg-blue-400"></span>
                                                    {format!("{} running agent{}", counts.agents, if counts.agents != 1 { "s" } else { "" })}
                                                </div>
                                            </Show>
                                        </div>
                                    </div>
                                }.into_any()
                            } else {
                                view! {
                                    <p class="text-white/60 text-sm mb-6">
                                        "SlashIt could not count the running terminals and agents, so it cannot confirm that nothing would be lost."
                                    </p>
                                }.into_any()
                            }}
                            <p class="text-white/40 text-xs mb-6">
                                "Restarting will terminate all running processes. The update is already installed and will be used on the next launch either way."
                            </p>
                        </div>
                        <div class="flex items-center justify-end gap-3 p-6 border-t border-white/5">
                            <button
                                on:click=move |_| ctx.restart_prompt.set(None)
                                class="px-4 py-2 rounded-lg text-white/70 hover:text-white/90 hover:bg-white/5 transition-colors"
                            >
                                "Cancel"
                            </button>
                            <button
                                on:click=move |_| perform_restart(ctx)
                                class="px-4 py-2 rounded-lg bg-red-500 hover:bg-red-600 text-white font-medium transition-colors"
                            >
                                "Restart anyway"
                            </button>
                        </div>
                    </div>
                </div>
            }
            .into_any()
        }}
    }
    .into_any()
}
