//! Deterministic fault injection and call tracing.
//!
//! State is per thread, so tests run in parallel without disturbing each
//! other, and an injected fault fires exactly once, at the first time the
//! write reaches the armed [`Stage`]. A write that runs on another thread
//! than the one that armed the fault is not affected.

use crate::Stage;
use std::cell::RefCell;
use std::ffi::OsString;
use std::io;
use std::path::Path;

thread_local! {
    static ARMED: RefCell<Option<(Stage, io::ErrorKind, Option<OsString>)>> = const { RefCell::new(None) };
    static TRACE: RefCell<Option<Vec<Stage>>> = const { RefCell::new(None) };
}

/// Disarms the fault when dropped.
pub struct FaultGuard(());

impl Drop for FaultGuard {
    fn drop(&mut self) {
        ARMED.with(|a| *a.borrow_mut() = None);
    }
}

/// Make the next write on this thread fail with `kind` when it reaches `stage`.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_at(stage: Stage, kind: io::ErrorKind) -> FaultGuard {
    ARMED.with(|a| *a.borrow_mut() = Some((stage, kind, None)));
    FaultGuard(())
}

/// Like [`fail_at`], but only for a write whose destination (or, for the
/// directory stages, directory) has this file name, so a test that performs
/// several writes fails exactly the one it means to.
#[must_use = "the fault is disarmed as soon as the guard is dropped"]
pub fn fail_at_file(stage: Stage, kind: io::ErrorKind, file_name: &str) -> FaultGuard {
    ARMED.with(|a| *a.borrow_mut() = Some((stage, kind, Some(OsString::from(file_name)))));
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
        let mut armed = a.borrow_mut();
        match *armed {
            Some((s, kind, ref only)) if s == stage && only.as_deref().is_none_or(|n| target.file_name() == Some(n)) => {
                *armed = None;
                Some(kind)
            }
            _ => None,
        }
    });
    match fire {
        Some(kind) => Err(io::Error::new(kind, format!("injected fault before {stage:?}"))),
        None => Ok(()),
    }
}
