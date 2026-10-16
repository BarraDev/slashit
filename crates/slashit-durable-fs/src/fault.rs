//! Deterministic fault injection and call tracing.
//!
//! State is per thread, so tests run in parallel without disturbing each
//! other, and an injected fault fires exactly once, at the first time the
//! write reaches the armed [`Stage`]. A write that runs on another thread
//! than the one that armed the fault is not affected.

use crate::Stage;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::Path;

/// One armed fault: where it fires, what it fails with, and how long it lasts.
struct Armed {
    stage: Stage,
    kind: io::ErrorKind,
    /// Limits the fault to a write whose target has this file name.
    only: Option<OsString>,
    /// Stays armed after firing, instead of firing once.
    every: bool,
    /// Matching steps to let through before the first failure.
    skip: u32,
}

impl Armed {
    fn new(stage: Stage, kind: io::ErrorKind) -> Self {
        Armed { stage, kind, only: None, every: false, skip: 0 }
    }
}

thread_local! {
    /// Faults waiting to fire, in order. Only the front one can fire, so a
    /// later step is armed only once the one before it has fired.
    static ARMED: RefCell<VecDeque<Armed>> = const { RefCell::new(VecDeque::new()) };
    static TRACE: RefCell<Option<Vec<Stage>>> = const { RefCell::new(None) };
}

/// Disarms the fault when dropped.
pub struct FaultGuard(());

impl Drop for FaultGuard {
    fn drop(&mut self) {
        ARMED.with(|a| a.borrow_mut().clear());
    }
}

fn arm(armed: Armed) -> FaultGuard {
    ARMED.with(|a| *a.borrow_mut() = VecDeque::from([armed]));
    FaultGuard(())
}

/// Make the next write on this thread fail with `kind` when it reaches `stage`.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_at(stage: Stage, kind: io::ErrorKind) -> FaultGuard {
    arm(Armed::new(stage, kind))
}

/// Like [`fail_at`], but lets the first `skip` times the writes on this thread
/// reach `stage` through and fails the one after, so a test can fail the
/// second write of an operation that makes two.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_after(stage: Stage, kind: io::ErrorKind, skip: u32) -> FaultGuard {
    arm(Armed { skip, ..Armed::new(stage, kind) })
}

/// [`fail_after`] that stays armed: the first `skip` writes reaching `stage`
/// succeed, and every write after them fails. A test uses it to fail an
/// operation's publishing write and the write that should follow it alike.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_every_after(stage: Stage, kind: io::ErrorKind, skip: u32) -> FaultGuard {
    arm(Armed { skip, every: true, ..Armed::new(stage, kind) })
}

/// Like [`fail_at`], but the fault stays armed until the guard is dropped:
/// every write on this thread that reaches `stage` fails, which is what a
/// filesystem that cannot perform that step looks like.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_every(stage: Stage, kind: io::ErrorKind) -> FaultGuard {
    arm(Armed { every: true, ..Armed::new(stage, kind) })
}

/// Like [`fail_at`], but only for a write whose destination (or, for the
/// directory stages, directory) has this file name, so a test that performs
/// several writes fails exactly the one it means to.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_at_file(stage: Stage, kind: io::ErrorKind, file_name: &str) -> FaultGuard {
    arm(Armed { only: Some(OsString::from(file_name)), ..Armed::new(stage, kind) })
}

/// Arms faults that fire in the order given, each once. A step waits until the
/// one before it has fired, and a write that reaches another stage in the
/// meantime is let through, so a test can fail two different writes of one
/// operation: the first step fails the first write's `Stage`, the second the
/// next write's. Each step is `(stage, kind, skip)`, where `skip` lets that many
/// matching writes through before the step fails.
#[must_use = "the faults are disarmed as soon as the guard is dropped"]
pub fn fail_in_order(steps: &[(Stage, io::ErrorKind, u32)]) -> FaultGuard {
    let queue = steps
        .iter()
        .map(|&(stage, kind, skip)| Armed { skip, ..Armed::new(stage, kind) })
        .collect();
    ARMED.with(|a| *a.borrow_mut() = queue);
    FaultGuard(())
}

/// Records every stage the writes on this thread reach, in order, until dropped.
pub struct Trace(());

impl Trace {
    pub fn stages(&self) -> Vec<Stage> {
        TRACE.with(|t| t.borrow().clone().unwrap_or_default())
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        TRACE.with(|t| *t.borrow_mut() = None);
    }
}

#[must_use = "tracing stops as soon as the guard is dropped"]
pub fn trace() -> Trace {
    TRACE.with(|t| *t.borrow_mut() = Some(Vec::new()));
    Trace(())
}

pub(crate) fn before(stage: Stage, target: &Path) -> io::Result<()> {
    TRACE.with(|t| {
        if let Some(stages) = t.borrow_mut().as_mut() {
            stages.push(stage);
        }
    });
    let fire = ARMED.with(|a| {
        let mut queue = a.borrow_mut();
        let armed = queue.front_mut()?;
        if armed.stage != stage || armed.only.as_deref().is_some_and(|n| target.file_name() != Some(n)) {
            return None;
        }
        if armed.skip > 0 {
            armed.skip -= 1;
            return None;
        }
        let kind = armed.kind;
        if !armed.every {
            queue.pop_front();
        }
        Some(kind)
    });
    match fire {
        Some(kind) => Err(io::Error::new(kind, format!("injected fault before {stage:?}"))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_fire_in_order_and_only_the_front_step_can_fire() {
        let _guard = fail_in_order(&[
            (Stage::SyncDirectory, io::ErrorKind::Other, 1),
            (Stage::Replace, io::ErrorKind::Other, 0),
        ]);
        let target = Path::new("board.json");

        assert!(before(Stage::SyncDirectory, target).is_ok(), "the first sync is skipped");
        assert!(before(Stage::Replace, target).is_ok(), "the second step is not yet armed");
        assert!(before(Stage::SyncDirectory, target).is_err(), "the sync step fires");
        assert!(before(Stage::Replace, target).is_err(), "then the replacement step fires");
        assert!(before(Stage::Replace, target).is_ok(), "nothing is left armed");
    }

    #[test]
    fn dropping_the_guard_disarms_every_remaining_step() {
        {
            let _guard = fail_in_order(&[
                (Stage::Replace, io::ErrorKind::Other, 0),
                (Stage::SyncDirectory, io::ErrorKind::Other, 0),
            ]);
        }
        let target = Path::new("board.json");
        assert!(before(Stage::Replace, target).is_ok());
        assert!(before(Stage::SyncDirectory, target).is_ok());
    }

    #[test]
    fn a_persistent_fault_keeps_firing_until_its_guard_drops() {
        let guard = fail_every(Stage::Replace, io::ErrorKind::Other);
        let target = Path::new("board.json");
        assert!(before(Stage::Replace, target).is_err());
        assert!(before(Stage::Replace, target).is_err());
        drop(guard);
        assert!(before(Stage::Replace, target).is_ok());
    }
}
