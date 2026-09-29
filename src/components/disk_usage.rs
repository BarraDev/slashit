//! Disk usage: how much space SlashIt uses, and how full the disk is.
//!
//! Informational only. There is deliberately no action here that deletes,
//! cleans or prunes anything; the numbers exist so the user can see them,
//! and so later policies have something trustworthy to stand on.
//!
//! Measuring walks SlashIt's directories, so it runs only when asked: once
//! when the section first opens with nothing measured yet, then on Refresh.
//! The previous result stays on screen while a new one is taken.

use crate::components::toast;
use crate::models::storage_usage::{
    iec_bytes, DiskPressure, Measurement, StorageConsumer, StorageStatus, StorageSummary,
};
use crate::services::storage_usage_service;
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::future::Future;
use wasm_bindgen::JsValue;

#[component]
pub fn DiskUsage() -> impl IntoView {
    let status = RwSignal::new(None::<StorageStatus>);
    let refreshing = RwSignal::new(false);

    let refresh = move || {
        if refreshing.get_untracked() {
            return;
        }
        refreshing.set(true);
        spawn_local(async move {
            let measured = storage_usage_service::refresh_storage_usage();
            if let Err(e) = refresh_into(measured, status, refreshing).await {
                toast::error(format!("Could not measure disk usage: {e}"));
            }
        });
    };

    let _ = Effect::new(move |prev: Option<bool>| {
        if prev.is_some() {
            return true;
        }
        spawn_local(async move {
            match load_into(storage_usage_service::get_storage_usage(), status, refreshing).await {
                Ok(true) => refresh(),
                Ok(false) => {}
                Err(e) => toast::error(format!("Could not read disk usage: {e}")),
            }
        });
        true
    });

    let measuring = move || refreshing.get() || status.get().is_some_and(|s| s.measuring);

    view! {
        <div class="space-y-4" data-testid="disk-usage">
            <div class="flex items-start justify-between gap-4">
                <div>
                    <h2 class="text-lg font-semibold text-white/90 mb-1">"Disk usage"</h2>
                    <p class="text-sm text-white/50">
                        "What SlashIt stores on this computer. Informational only: SlashIt does not delete, clean up or limit anything based on these numbers."
                    </p>
                </div>
                <button
                    data-testid="disk-usage-refresh"
                    disabled=measuring
                    on:click=move |_| refresh()
                    class="shrink-0 px-3 py-1.5 rounded-lg text-sm bg-white/10 text-white/80 hover:bg-white/15 disabled:opacity-50"
                >
                    {move || if measuring() { "Measuring…" } else { "Refresh" }}
                </button>
            </div>

            {move || {
                let current = status.get();
                let last_error = current.as_ref().and_then(|s| s.last_error.clone());
                let error_view = last_error.map(|e| view! {
                    <div class="p-3 rounded-lg bg-red-500/10 border border-red-500/20">
                        <p class="text-sm text-red-200/90">{format!("The last measurement failed: {e}")}</p>
                    </div>
                });
                let body = match current.and_then(|s| s.summary) {
                    Some(summary) => summary_view(summary).into_any(),
                    None => view! {
                        <p class="text-sm text-white/40">
                            {move || if measuring() { "Measuring…" } else { "Not measured yet." }}
                        </p>
                    }.into_any(),
                };
                view! { <div class="space-y-4">{error_view}{body}</div> }
            }}
        </div>
    }
}

/// Show the measurement a Refresh answers with, then let Refresh be pressed
/// again.
async fn refresh_into(
    measured: impl Future<Output = Result<StorageStatus, String>>,
    status: RwSignal<Option<StorageStatus>>,
    refreshing: RwSignal<bool>,
) -> Result<(), String> {
    let shown = measured.await.map(|latest| status.set(Some(latest)));
    refreshing.set(false);
    shown
}

/// Show the latest measurement when the section opens. Returns whether the
/// view should refresh.
async fn load_into(
    read: impl Future<Output = Result<StorageStatus, String>>,
    status: RwSignal<Option<StorageStatus>>,
    refreshing: RwSignal<bool>,
) -> Result<bool, String> {
    let current = read.await?;
    // Nothing measured yet, or a measurement already running that this view
    // should wait for: either way, refresh (which joins a running one rather
    // than starting another).
    let wait = current.summary.is_none() || current.measuring;
    // A Refresh pressed while this read was in flight answers with something
    // at least as new, whether it is still running or has already shown its
    // result. Only this read and a Refresh set `status`, so anything already
    // there came from that Refresh.
    if refreshing.get_untracked() || status.with_untracked(Option::is_some) {
        return Ok(false);
    }
    status.set(Some(current));
    Ok(wait)
}

fn summary_view(summary: StorageSummary) -> impl IntoView {
    let measured_at = local_time(&summary.measured_at);
    let duration = summary.duration_ms;

    let space = match (summary.filesystem, summary.pressure) {
        (Some(fs), Some(pressure)) => {
            let (badge, note) = pressure_style(pressure);
            let thresholds = summary.thresholds.map(|t| {
                format!(
                    "Warning below {} free, critical below {} free.",
                    iec_bytes(t.warning_below_bytes),
                    iec_bytes(t.critical_below_bytes)
                )
            });
            view! {
                <div class="p-4 rounded-xl bg-white/5 border border-white/10 space-y-2">
                    <div class="flex items-center justify-between gap-3">
                        <p class="text-white/90 font-medium" data-testid="disk-usage-free">
                            {format!("{} free of {}", iec_bytes(fs.available_bytes), iec_bytes(fs.total_bytes))}
                        </p>
                        <span
                            data-testid="disk-usage-pressure"
                            data-pressure=pressure.label().to_lowercase()
                            class=format!("px-2 py-0.5 rounded-md text-xs font-medium {badge}")
                        >
                            {pressure.label()}
                        </span>
                    </div>
                    <p class="text-xs text-white/40">
                        {format!("{note} {}", thresholds.unwrap_or_default())}
                    </p>
                </div>
            }
            .into_any()
        }
        _ => view! {
            <div class="p-4 rounded-xl bg-amber-500/10 border border-amber-500/20">
                <p class="text-sm text-amber-200/90" data-testid="disk-usage-free">
                    {format!(
                        "Free space could not be read{}. Disk pressure is unknown.",
                        summary.filesystem_error.map(|e| format!(": {e}")).unwrap_or_default()
                    )}
                </p>
            </div>
        }
        .into_any(),
    };

    let totals = [
        (
            "owned",
            "Owned by SlashIt",
            summary.slashit_owned_bytes,
            "Everything below SlashIt's own folders that it can account for.",
        ),
        (
            "active",
            "Active task checkouts",
            summary.active_workspace_bytes,
            "Checkouts of tasks still on the board and not Done.",
        ),
        (
            "rebuildable",
            "Rebuildable",
            summary.rebuildable_bytes,
            "Build output (target/, dist/) that git ignores in task checkouts. A build recreates it.",
        ),
        (
            "reclaimable",
            "Could be freed",
            summary.reclaimable_bytes,
            "The rebuildable part in checkouts of tasks that are not running, queued or in AI review, with no agent attached when measured. SlashIt does not remove it.",
        ),
        (
            "unknown",
            "Unrecognized",
            summary.unknown_managed_bytes,
            "Inside SlashIt's folders but not attributable to anything, such as a checkout no task records. Never counted as reclaimable.",
        ),
    ];

    let incomplete = summary.incomplete.then(|| view! {
        <p class="text-xs text-amber-200/80" data-testid="disk-usage-incomplete">
            "Some items could not be measured completely, so these totals are lower bounds."
        </p>
    });
    let skipped = (summary.links_not_followed > 0 || summary.mounts_not_entered > 0).then(|| view! {
        <p class="text-xs text-white/40">
            {format!(
                "Not followed: {} link(s) and {} folder(s) on other disks inside SlashIt's folders. What they point to is not counted.",
                summary.links_not_followed,
                summary.mounts_not_entered
            )}
        </p>
    });
    let external = (summary.external_checkouts > 0).then(|| view! {
        <p class="text-xs text-white/40">
            {format!(
                "{} task checkout(s) are outside SlashIt's folders and are not measured.",
                summary.external_checkouts
            )}
        </p>
    });

    let consumers = summary.largest_consumers;
    let unmeasured = summary.unmeasured;

    view! {
        <div class="space-y-4">
            {space}

            <div class="grid grid-cols-2 gap-3">
                {totals.into_iter().map(|(id, label, bytes, hint)| view! {
                    <div class="p-3 rounded-lg bg-white/[0.03] border border-white/10" title=hint>
                        <p class="text-xs text-white/40">{label}</p>
                        <p class="text-white/90 font-medium" data-testid=format!("disk-usage-{id}")>{iec_bytes(bytes)}</p>
                        <p class="text-xs text-white/30 mt-1">{hint}</p>
                    </div>
                }).collect::<Vec<_>>()}
            </div>

            {incomplete}
            {skipped}
            {external}

            <div>
                <h3 class="text-sm font-medium text-white/70 mb-2">"Largest items"</h3>
                {if consumers.is_empty() {
                    view! { <p class="text-sm text-white/40">"Nothing stored yet."</p> }.into_any()
                } else {
                    view! {
                        <ul class="divide-y divide-white/5 rounded-lg border border-white/10" data-testid="disk-usage-consumers">
                            {consumers.into_iter().map(consumer_row).collect::<Vec<_>>()}
                        </ul>
                    }.into_any()
                }}
            </div>

            {(!unmeasured.is_empty()).then(|| view! {
                <div>
                    <h3 class="text-sm font-medium text-white/70 mb-2">"Could not be measured"</h3>
                    <ul class="divide-y divide-white/5 rounded-lg border border-amber-500/20">
                        {unmeasured.into_iter().map(consumer_row).collect::<Vec<_>>()}
                    </ul>
                </div>
            })}

            <p class="text-xs text-white/30" data-testid="disk-usage-measured-at">
                {format!("Measured {measured_at}, in {duration} ms.")}
            </p>
            <p class="text-xs text-white/30">
                "Not included: your repositories, boards kept inside a project, git's own data in each repository, and shared tool caches such as Cargo's."
            </p>
        </div>
    }
}

fn consumer_row(consumer: StorageConsumer) -> impl IntoView {
    let size = match (&consumer.bytes, &consumer.measurement) {
        (Some(bytes), Measurement::Partial { .. }) => format!("at least {}", iec_bytes(*bytes)),
        (Some(bytes), _) => iec_bytes(*bytes),
        (None, _) => "not measured".to_string(),
    };
    let problem = match &consumer.measurement {
        Measurement::Complete => None,
        Measurement::Partial { reason, .. } | Measurement::Failed { reason } => Some(reason.clone()),
    };
    let mut tags = vec![consumer.classification.label().to_string()];
    if let Some(lifecycle) = &consumer.lifecycle {
        tags.push(lifecycle.label().to_string());
    }
    if consumer.reclaimable {
        tags.push("could be freed".to_string());
    }
    let secondary = consumer.detail.clone().into_iter().chain(tags).collect::<Vec<_>>().join(" · ");

    view! {
        <li class="flex items-center justify-between gap-3 px-3 py-2" title=problem.unwrap_or_default()>
            <div class="min-w-0">
                <p class="text-sm text-white/80 truncate">{consumer.label}</p>
                <p class="text-xs text-white/40 truncate">{secondary}</p>
            </div>
            <span class="shrink-0 text-sm text-white/70 tabular-nums">{size}</span>
        </li>
    }
}

fn pressure_style(pressure: DiskPressure) -> (&'static str, &'static str) {
    match pressure {
        DiskPressure::Normal => ("bg-green-500/20 text-green-300", "Plenty of free space."),
        DiskPressure::Warning => (
            "bg-amber-500/20 text-amber-300",
            "Free space is getting low. Nothing is limited because of it.",
        ),
        DiskPressure::Critical => (
            "bg-red-500/20 text-red-300",
            "Free space is very low. Nothing is limited because of it.",
        ),
    }
}

fn local_time(at: &chrono::DateTime<chrono::Utc>) -> String {
    let date = js_sys::Date::new(&JsValue::from_f64(at.timestamp_millis() as f64));
    String::from(date.to_locale_string("en-US", &JsValue::UNDEFINED))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::oneshot;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;

    /// A status measured at `at`, or with nothing measured yet.
    fn status_at(at: Option<&str>, measuring: bool) -> StorageStatus {
        let summary = at.map(|at| {
            serde_json::from_value(serde_json::json!({
                "measured_at": at,
                "duration_ms": 1,
                "filesystem": null,
                "filesystem_error": null,
                "pressure": null,
                "thresholds": null,
                "slashit_owned_bytes": 0,
                "active_workspace_bytes": 0,
                "rebuildable_bytes": 0,
                "reclaimable_bytes": 0,
                "unknown_managed_bytes": 0,
                "incomplete": false,
                "links_not_followed": 0,
                "mounts_not_entered": 0,
                "external_checkouts": 0,
                "largest_consumers": [],
                "unmeasured": []
            }))
            .expect("a summary")
        });
        StorageStatus { summary, measuring, last_error: None }
    }

    #[test]
    fn an_opening_read_answering_after_a_refresh_does_not_replace_it() {
        let status = RwSignal::new(None::<StorageStatus>);
        let refreshing = RwSignal::new(false);
        let mut pool = LocalPool::new();
        let spawner = pool.spawner();

        // The section opens; its read is held while a measurement is running.
        let (answer_read, read) = oneshot::channel();
        let opening = std::rc::Rc::new(std::cell::Cell::new(None));
        let opened = opening.clone();
        spawner
            .spawn_local(async move {
                let read = async { read.await.expect("the read is answered") };
                opened.set(Some(load_into(read, status, refreshing).await));
            })
            .unwrap();
        pool.run_until_stalled();
        assert_eq!(opening.take(), None, "the read is still in flight");

        // Refresh is pressed and finishes with a newer measurement.
        let newer = status_at(Some("2026-09-29T12:00:00Z"), false);
        refreshing.set(true);
        let measured = futures::future::ready(Ok(newer.clone()));
        spawner
            .spawn_local(async move {
                refresh_into(measured, status, refreshing).await.unwrap();
            })
            .unwrap();
        pool.run_until_stalled();
        assert!(!refreshing.get_untracked());
        assert_eq!(status.get_untracked(), Some(newer.clone()));

        // Only now does the opening read answer, with what it saw before.
        answer_read.send(Ok(status_at(None, true))).unwrap();
        pool.run_until_stalled();
        assert_eq!(status.get_untracked(), Some(newer), "the newer measurement stays");
        assert_eq!(opening.take(), Some(Ok(false)), "no second refresh");
    }

    #[test]
    fn an_opening_read_is_shown_when_nothing_else_answered_first() {
        let status = RwSignal::new(None::<StorageStatus>);
        let refreshing = RwSignal::new(false);
        let current = status_at(Some("2026-09-29T12:00:00Z"), false);
        let wait = futures::executor::block_on(load_into(
            futures::future::ready(Ok(current.clone())),
            status,
            refreshing,
        ));
        assert_eq!(wait, Ok(false));
        assert_eq!(status.get_untracked(), Some(current));

        let status = RwSignal::new(None::<StorageStatus>);
        let wait = futures::executor::block_on(load_into(
            futures::future::ready(Ok(status_at(None, false))),
            status,
            refreshing,
        ));
        assert_eq!(wait, Ok(true), "nothing measured yet, so refresh");
    }
}
