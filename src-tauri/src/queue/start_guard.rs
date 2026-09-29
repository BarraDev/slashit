//! Whether SlashIt may begin a new task execution, as far as disk space goes.
//!
//! A task execution creates or reattaches a Task Checkout and then runs an
//! agent that builds in it, so it is the thing that grows the disk. When the
//! filesystem holding SlashIt's data directory is critically low, new
//! executions wait; everything already running carries on. This is a start
//! guard, not a capacity limit: it is separate from
//! [`Admission`](crate::queue::admission::Admission), which answers how many
//! agent flows may run at once, and it never touches a permit already held.
//!
//! Every check asks the filesystem afresh with one `statvfs`-style call. It
//! never reads the Storage view's last measurement, which may be old, and
//! never walks a directory tree.
//!
//! What pauses new work is [`PressurePolicy::start_block`]: Critical does,
//! Warning does not, and a filesystem that cannot be asked does too, since a
//! start that cannot prove there is room is not safe to make.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use crate::domain::storage_usage::{FilesystemSpace, PressurePolicy, StartBlock};
use crate::domain::TaskStatus;

/// Reads the free space of the filesystem new executions would grow.
pub type SpaceProbe = Arc<dyn Fn() -> io::Result<FilesystemSpace> + Send + Sync>;

/// Debug builds only: a file whose contents replace the real filesystem
/// reading, so the desktop acceptance journeys can pause and resume new work
/// without depending on the host's disk. The file holds two numbers, total
/// and available bytes; anything else reads as a failed check. Release
/// builds ignore the variable.
pub const DEBUG_DISK_SPACE_FILE: &str = "SLASHIT_DEBUG_DISK_SPACE_FILE";

/// How long a check waits for the filesystem before treating free space as
/// unknown. A local `statvfs` answers in microseconds; a stalled network
/// mount may never answer.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub struct StartGuard {
    probe: SpaceProbe,
    policy: PressurePolicy,
    timeout: std::time::Duration,
    /// One reading at a time. A probe stuck on a stalled mount then occupies
    /// one blocking thread, rather than one more for every caller that asks
    /// while it is stuck.
    in_flight: Arc<tokio::sync::Mutex<()>>,
    /// Which way the previous check decided, so a change is logged once
    /// rather than on every scheduler pass.
    last: std::sync::Mutex<Option<Option<std::mem::Discriminant<StartBlock>>>>,
}

impl StartGuard {
    pub fn new(probe: SpaceProbe) -> Self {
        Self {
            probe,
            policy: PressurePolicy::default(),
            timeout: CHECK_TIMEOUT,
            in_flight: Arc::new(tokio::sync::Mutex::new(())),
            last: std::sync::Mutex::new(None),
        }
    }

    /// The guard the application runs: the filesystem holding `data_dir`,
    /// the same one Settings > Storage reports pressure for.
    pub fn for_data_dir(data_dir: PathBuf) -> Self {
        #[cfg(debug_assertions)]
        if let Some(file) = std::env::var_os(DEBUG_DISK_SPACE_FILE) {
            let file = PathBuf::from(file);
            return Self::new(Arc::new(move || debug_space(&file)));
        }
        Self::new(Arc::new(move || crate::storage_accounting::filesystem_space(&data_dir)))
    }

    /// Whether a new task execution may begin right now.
    pub async fn check(&self) -> Result<(), StartBlock> {
        let space = match tokio::time::timeout(self.timeout, self.read()).await {
            Ok(space) => space,
            Err(_) => Err(format!(
                "the filesystem did not answer within {:.1}s",
                self.timeout.as_secs_f64()
            )),
        };
        let block = self.policy.start_block(space);
        self.log_change(&block);
        match block {
            None => Ok(()),
            Some(block) => Err(block),
        }
    }

    async fn read(&self) -> Result<FilesystemSpace, String> {
        // Owned, so a reading abandoned by the timeout still holds the slot
        // until its blocking call actually returns.
        let slot = self.in_flight.clone().lock_owned().await;
        let probe = self.probe.clone();
        // Off the async runtime: a statvfs on a stalled network mount blocks.
        match tokio::task::spawn_blocking(move || {
            let _slot = slot;
            probe()
        })
        .await
        {
            Ok(Ok(space)) => Ok(space),
            Ok(Err(e)) => Err(e.to_string()),
            Err(e) => Err(format!("the check stopped unexpectedly: {e}")),
        }
    }

    /// Refuse a move into In Progress while new work is paused. Moving there
    /// is what asks the executor to start the task, so it is refused before
    /// anything is written, and before any current owner is ended. Every
    /// other move, including into Queue, is unaffected: a queued task simply
    /// waits.
    pub async fn check_move(&self, from: &TaskStatus, to: &TaskStatus) -> Result<(), String> {
        if *to != TaskStatus::InProgress || *from == TaskStatus::InProgress {
            return Ok(());
        }
        self.check().await.map_err(|block| block.to_string())
    }

    fn log_change(&self, block: &Option<StartBlock>) {
        let kind = block.as_ref().map(std::mem::discriminant);
        let mut last = match self.last.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *last == Some(kind) {
            return;
        }
        match block {
            Some(block) => eprintln!("[start-guard] {block}"),
            // The first check of a healthy disk is not news.
            None if last.is_some() => eprintln!("[start-guard] new work resumed"),
            None => {}
        }
        *last = Some(kind);
    }
}

#[cfg(debug_assertions)]
fn debug_space(file: &std::path::Path) -> io::Result<FilesystemSpace> {
    let text = std::fs::read_to_string(file)?;
    let mut numbers = text.split_whitespace().map(str::parse::<u64>);
    match (numbers.next(), numbers.next(), numbers.next()) {
        (Some(Ok(total_bytes)), Some(Ok(available_bytes)), None) => {
            Ok(FilesystemSpace { total_bytes, available_bytes })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} does not hold two byte counts", file.display()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::storage_usage::GIB;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn guard_reading(available: Arc<AtomicU64>) -> StartGuard {
        StartGuard::new(Arc::new(move || {
            Ok(FilesystemSpace {
                total_bytes: 500 * GIB,
                available_bytes: available.load(Ordering::SeqCst),
            })
        }))
    }

    #[tokio::test]
    async fn every_check_reads_the_filesystem_again() {
        let available = Arc::new(AtomicU64::new(10 * GIB));
        let guard = guard_reading(available.clone());
        assert!(matches!(guard.check().await, Err(StartBlock::CriticalDisk { .. })));
        available.store(200 * GIB, Ordering::SeqCst);
        assert_eq!(guard.check().await, Ok(()));
    }

    #[tokio::test]
    async fn a_failed_reading_pauses_new_work_as_unknown_not_critical() {
        let guard = StartGuard::new(Arc::new(|| Err(io::Error::other("statvfs failed"))));
        match guard.check().await {
            Err(StartBlock::DiskSpaceUnavailable { reason }) => {
                assert!(reason.contains("statvfs failed"), "{reason}")
            }
            other => panic!("expected an unknown-space block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_filesystem_that_never_answers_pauses_new_work_and_holds_one_thread() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (release, released) = std::sync::mpsc::channel::<()>();
        let released = Arc::new(std::sync::Mutex::new(released));
        let counted = calls.clone();
        let mut guard = StartGuard::new(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = released.lock().unwrap().recv();
            Err(io::Error::other("released"))
        }));
        guard.timeout = std::time::Duration::from_millis(50);

        for _ in 0..3 {
            match guard.check().await {
                Err(StartBlock::DiskSpaceUnavailable { reason }) => {
                    assert!(reason.contains("did not answer"), "{reason}")
                }
                other => panic!("expected an unknown-space block, got {other:?}"),
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "later checks wait for the stuck one");
        drop(release);
    }

    #[tokio::test]
    async fn only_a_move_into_in_progress_is_checked() {
        let guard = guard_reading(Arc::new(AtomicU64::new(0)));
        use TaskStatus::*;
        for (from, to) in [
            (Backlog, Queue),
            (Error, Queue),
            (InProgress, InProgress),
            (InProgress, AiReview),
            (AiReview, HumanReview),
            (HumanReview, Queue),
            (Queue, Backlog),
        ] {
            assert_eq!(guard.check_move(&from, &to).await, Ok(()), "{from:?} -> {to:?}");
        }
        for from in [Backlog, Queue, Error, AiReview, HumanReview, Done] {
            let refused = guard.check_move(&from, &InProgress).await;
            assert!(
                refused.as_ref().is_err_and(|m| m.starts_with("New work paused")),
                "{from:?} -> InProgress: {refused:?}"
            );
        }
    }

    #[test]
    fn the_real_probe_reads_an_existing_directory_and_fails_on_a_missing_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let space = crate::storage_accounting::filesystem_space(dir.path()).expect("statvfs");
        assert!(space.total_bytes > 0);
        assert!(crate::storage_accounting::filesystem_space(&dir.path().join("missing")).is_err());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_debug_override_reads_two_byte_counts_and_nothing_else() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("space");
        std::fs::write(&file, "1000 250\n").unwrap();
        assert_eq!(
            debug_space(&file).unwrap(),
            FilesystemSpace { total_bytes: 1000, available_bytes: 250 }
        );
        for bad in ["", "1000", "1000 x", "1 2 3"] {
            std::fs::write(&file, bad).unwrap();
            assert!(debug_space(&file).is_err(), "{bad:?}");
        }
        assert!(debug_space(&dir.path().join("missing")).is_err());
    }
}
