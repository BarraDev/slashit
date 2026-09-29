//! At most one measurement at a time, however many ask.
//!
//! Walking SlashIt's directories can take a while, so nothing measures on a
//! timer or on every view: a measurement runs when one is asked for. A
//! request that arrives while one is running does not start another; it
//! waits for the running one and gets its result. The previous result stays
//! readable the whole time.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::watch;

use crate::domain::storage_usage::{StorageStatus, StorageSummary};

/// Produces one measurement. Called only while no other is running.
pub type Measure = Box<dyn Fn() -> BoxFuture<'static, Result<StorageSummary, String>> + Send + Sync>;

pub struct StorageAccounting {
    shared: Arc<Shared>,
    measure: Measure,
}

struct Shared {
    progress: Mutex<Progress>,
    /// Bumped each time a measurement finishes, successfully or not.
    finished: watch::Sender<u64>,
}

#[derive(Default)]
struct Progress {
    measuring: bool,
    latest: Option<StorageSummary>,
    last_error: Option<String>,
}

impl StorageAccounting {
    pub fn new(measure: Measure) -> Self {
        Self {
            shared: Arc::new(Shared {
                progress: Mutex::new(Progress::default()),
                finished: watch::Sender::new(0),
            }),
            measure,
        }
    }

    /// The latest result, without measuring anything.
    pub fn status(&self) -> StorageStatus {
        self.shared.status()
    }

    /// Measure, or join the measurement already running, and return the
    /// status once it has finished.
    pub async fn refresh(&self) -> StorageStatus {
        let mut finished = {
            let mut progress = self.shared.lock();
            // Subscribed under the same lock the finishing measurement
            // takes to publish, so its completion cannot slip past between
            // the check and the wait.
            let finished = self.shared.finished.subscribe();
            if !progress.measuring {
                progress.measuring = true;
                self.start();
            }
            finished
        };
        // Only fails if the sender is gone, which `self` rules out.
        let _ = finished.changed().await;
        self.status()
    }

    fn start(&self) {
        let shared = self.shared.clone();
        let measurement = (self.measure)();
        tokio::spawn(async move {
            // Its own task, so a panic inside it is reported here as an
            // error rather than leaving `measuring` set forever.
            let outcome = match tokio::spawn(measurement).await {
                Ok(outcome) => outcome,
                Err(e) => Err(format!("the measurement stopped unexpectedly: {e}")),
            };
            let mut progress = shared.lock();
            match outcome {
                Ok(summary) => {
                    progress.latest = Some(summary);
                    progress.last_error = None;
                }
                Err(e) => progress.last_error = Some(e),
            }
            progress.measuring = false;
            shared.finished.send_modify(|count| *count = count.wrapping_add(1));
        });
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Progress> {
        // Nothing panics while holding it; recover the data regardless.
        self.progress.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn status(&self) -> StorageStatus {
        let progress = self.lock();
        StorageStatus {
            summary: progress.latest.clone(),
            measuring: progress.measuring,
            last_error: progress.last_error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    fn summary(owned: u64) -> StorageSummary {
        StorageSummary {
            measured_at: chrono::Utc::now(),
            duration_ms: 0,
            filesystem: None,
            filesystem_error: None,
            pressure: None,
            thresholds: None,
            slashit_owned_bytes: owned,
            active_workspace_bytes: 0,
            rebuildable_bytes: 0,
            reclaimable_bytes: 0,
            unknown_managed_bytes: 0,
            incomplete: false,
            links_not_followed: 0,
            mounts_not_entered: 0,
            external_checkouts: 0,
            largest_consumers: Vec::new(),
            unmeasured: Vec::new(),
        }
    }

    /// A measurement that counts how often it starts and finishes only when
    /// the test lets it.
    fn gated() -> (StorageAccounting, Arc<AtomicUsize>, Arc<Semaphore>) {
        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let (s, g) = (started.clone(), gate.clone());
        let accounting = StorageAccounting::new(Box::new(move || {
            let (s, g) = (s.clone(), g.clone());
            Box::pin(async move {
                let n = s.fetch_add(1, Ordering::SeqCst) + 1;
                g.acquire().await.unwrap().forget();
                Ok(summary(n as u64))
            })
        }));
        (accounting, started, gate)
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_measurement() {
        let (accounting, started, gate) = gated();
        let accounting = Arc::new(accounting);

        let waiters: Vec<_> = (0..16)
            .map(|_| {
                let a = accounting.clone();
                tokio::spawn(async move { a.refresh().await })
            })
            .collect();
        // Let every request arrive while the measurement is held open.
        while started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(accounting.status().measuring);
        assert_eq!(started.load(Ordering::SeqCst), 1);

        gate.add_permits(1);
        for waiter in waiters {
            let status = waiter.await.unwrap();
            assert_eq!(status.summary.unwrap().slashit_owned_bytes, 1);
        }
        assert_eq!(started.load(Ordering::SeqCst), 1, "no request started a second walk");
        assert!(!accounting.status().measuring);
    }

    #[tokio::test]
    async fn the_previous_result_stays_visible_while_measuring_again() {
        let (accounting, _started, gate) = gated();
        let accounting = Arc::new(accounting);
        assert!(accounting.status().summary.is_none());

        gate.add_permits(1);
        assert_eq!(accounting.refresh().await.summary.unwrap().slashit_owned_bytes, 1);

        let a = accounting.clone();
        let second = tokio::spawn(async move { a.refresh().await });
        while !accounting.status().measuring {
            tokio::task::yield_now().await;
        }
        let during = accounting.status();
        assert_eq!(during.summary.unwrap().slashit_owned_bytes, 1);

        gate.add_permits(1);
        assert_eq!(second.await.unwrap().summary.unwrap().slashit_owned_bytes, 2);
    }

    #[tokio::test]
    async fn a_failed_or_panicking_measurement_keeps_the_last_result_and_frees_the_slot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let accounting = StorageAccounting::new(Box::new(move || {
            let n = c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match n {
                    0 => Ok(summary(7)),
                    1 => Err("disk vanished".to_string()),
                    _ => panic!("walker bug"),
                }
            })
        }));

        assert_eq!(accounting.refresh().await.summary.unwrap().slashit_owned_bytes, 7);

        let failed = accounting.refresh().await;
        assert_eq!(failed.last_error.as_deref(), Some("disk vanished"));
        assert_eq!(failed.summary.unwrap().slashit_owned_bytes, 7);
        assert!(!failed.measuring);

        let panicked = accounting.refresh().await;
        assert!(panicked.last_error.unwrap().contains("stopped unexpectedly"));
        assert_eq!(panicked.summary.unwrap().slashit_owned_bytes, 7);
        assert!(!panicked.measuring, "a panic must not leave the slot taken");
    }
}
