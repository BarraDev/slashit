//! One backend-owned capacity gate shared by ordinary task execution and AI
//! review/fix work.
//!
//! Before this module, "how many agent-owning flows may run at once" had two
//! independent, disagreeing answers: `TaskExecutor::check_and_execute` capped
//! new executions by counting live `running_handles` entries, and
//! `QueueManager::select_promotable` capped `Queue -> InProgress` promotion by
//! counting tasks whose *persisted status* was `InProgress`. Neither counted a
//! running AI review or fix agent at all, so `parallel_task_limit` executions
//! plus an unbounded number of concurrent reviews could all hold a real
//! `claude` process at once.
//!
//! [`Admission`] is the one authoritative resource both paths now go through.
//! A permit is not a dashboard count sampled after the fact -- it is acquired
//! before a flow is allowed to start, and it is not returned to the pool until
//! whatever holds it (an execution's spawned future, a review's spawned
//! future, covering the reviewer agent, the fix agent and everything between
//! them) actually finishes. Removing a task from `running_handles` or
//! `reviewing_handles` does not by itself free capacity; dropping the
//! [`AdmissionPermit`] does, and the permit is only dropped once the future
//! that owns it returns.

use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

/// Shared internal state, split out from [`Admission`] only so
/// [`AdmissionPermit`]'s `Drop` impl can reach it without also holding a
/// second `Arc` to the semaphore.
struct AdmissionInner {
    semaphore: Arc<Semaphore>,
    /// The capacity the semaphore is currently configured to represent --
    /// `Semaphore` has no `permits()` getter, so this is tracked alongside it.
    total: Mutex<usize>,
    /// Capacity a shrink could not take back immediately because it was held
    /// by in-flight work. Resolved permit-by-permit as each one is dropped --
    /// see [`AdmissionPermit::drop`] -- rather than all at once, so a shrink
    /// never revokes capacity from work that is already running.
    ///
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: `Drop` cannot `.await`,
    /// so the release path needs a lock it can always actually take. An
    /// earlier version used `try_lock` here and silently skipped the
    /// decrement -- returning capacity that was supposed to stay owed -- on
    /// the rare release that raced `reconcile`'s own brief hold of this same
    /// lock. The critical sections on both sides are a few instructions with
    /// no `.await` inside them, so a blocking lock here never stalls the
    /// async runtime.
    pending_shrink: std::sync::Mutex<usize>,
}

/// One shared admission gate for active agent work (execution and AI
/// review/fix alike).
#[derive(Clone)]
pub struct Admission {
    inner: Arc<AdmissionInner>,
}

impl Admission {
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(AdmissionInner {
                semaphore: Arc::new(Semaphore::new(limit)),
                total: Mutex::new(limit),
                pending_shrink: std::sync::Mutex::new(0),
            }),
        }
    }

    /// Reconcile the gate's capacity with a freshly-read runtime
    /// `parallel_task_limit`.
    ///
    /// Growing first cancels any still-`pending_shrink` debt (each unit
    /// cancelled means the in-flight permit it was owed against goes back to
    /// behaving normally on release, with no physical change to the
    /// semaphore needed) and only calls `add_permits` for whatever growth
    /// remains beyond that -- so new admission respects the latest limit on
    /// the very next `try_acquire` in both cases: capacity a shrink had
    /// already physically reclaimed, and capacity a shrink had only queued to
    /// reclaim later. Without cancelling the debt first, growing back up
    /// after a shrink that had anything still pending would leave that many
    /// permits owed to be forgotten from unrelated future releases,
    /// permanently under-provisioning relative to the new limit.
    ///
    /// Shrinking takes back whatever is currently idle (not held by any
    /// in-flight execution or review) right away, and queues the remainder as
    /// [`AdmissionInner::pending_shrink`]: each of *those* permits is taken
    /// back the moment the flow holding it finishes, rather than the flow
    /// being killed to enforce the new limit early. Lowering the configured
    /// limit must never kill work already admitted.
    pub async fn reconcile(&self, new_limit: usize) {
        let mut total = self.inner.total.lock().await;
        if new_limit > *total {
            let mut grow = new_limit - *total;
            *total = new_limit;
            let cancelled = {
                let mut pending = self.inner.pending_shrink.lock().unwrap();
                let cancel = grow.min(*pending);
                *pending -= cancel;
                cancel
            };
            grow -= cancelled;
            if grow > 0 {
                self.inner.semaphore.add_permits(grow);
            }
        } else if new_limit < *total {
            let mut shrink_by = *total - new_limit;
            *total = new_limit;
            // Reclaim idle capacity immediately.
            while shrink_by > 0 {
                match self.inner.semaphore.try_acquire() {
                    Ok(permit) => {
                        permit.forget();
                        shrink_by -= 1;
                    }
                    Err(_) => break, // nothing idle left; queue the rest
                }
            }
            if shrink_by > 0 {
                *self.inner.pending_shrink.lock().unwrap() += shrink_by;
            }
        }
    }

    /// Take a permit if one is free right now, or decline.
    ///
    /// Non-blocking on purpose: every caller already has a well-defined
    /// "declining is free, the next pass tries again" fallback (the poller's
    /// 3-second tick, or a manual command telling the user to ask again
    /// shortly), and there is no queue of waiters to be fair between here --
    /// fairness between execution and review is a *scheduling* decision the
    /// caller makes about *when* to call `try_acquire`, not something this
    /// gate arbitrates.
    pub fn try_acquire(&self) -> Option<AdmissionPermit> {
        Arc::clone(&self.inner.semaphore)
            .try_acquire_owned()
            .ok()
            .map(|permit| AdmissionPermit {
                inner: Some(permit),
                admission: self.inner.clone(),
            })
    }
}

/// Proof of one reserved slot of active-agent-work capacity.
///
/// Held for exactly as long as the execution or review flow it belongs to is
/// alive. Moving it into the `tokio::spawn`ed future -- not just holding it
/// across the synchronous call that starts that future -- is what makes
/// "removing a handle-map entry" and "returning capacity" two different
/// events: the permit outlives every intermediate bookkeeping step and is
/// only dropped by the future's own final statement.
pub struct AdmissionPermit {
    inner: Option<OwnedSemaphorePermit>,
    admission: Arc<AdmissionInner>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let Some(permit) = self.inner.take() else {
            return;
        };
        // `pending_shrink` is only ever resolved here, one permit per drop,
        // so a shrink converges exactly as fast as in-flight work naturally
        // finishes -- never faster (nothing here kills a running flow) and
        // never slower (every single release checks in, deterministically:
        // this is a real blocking lock, not a `try_lock` that could silently
        // skip the decrement under contention -- see the field doc).
        let mut pending = self.admission.pending_shrink.lock().unwrap();
        if *pending > 0 {
            *pending -= 1;
            permit.forget();
            return;
        }
        drop(pending);
        drop(permit); // ordinary return to the pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn grants_up_to_the_configured_limit_and_then_declines() {
        let admission = Admission::new(2);
        let a = admission.try_acquire().expect("first permit");
        let b = admission.try_acquire().expect("second permit");
        assert!(
            admission.try_acquire().is_none(),
            "a third permit must be refused at limit 2"
        );
        drop(a);
        assert!(
            admission.try_acquire().is_some(),
            "dropping a held permit must free capacity for a new acquire"
        );
        drop(b);
    }

    #[tokio::test]
    async fn growing_the_limit_admits_more_immediately() {
        let admission = Admission::new(1);
        let _a = admission.try_acquire().expect("first permit");
        assert!(admission.try_acquire().is_none());
        admission.reconcile(2).await;
        assert!(
            admission.try_acquire().is_some(),
            "raising the limit must admit a second concurrent permit"
        );
    }

    #[tokio::test]
    async fn shrinking_never_revokes_a_permit_already_held() {
        let admission = Admission::new(2);
        let a = admission.try_acquire().expect("first permit");
        let b = admission.try_acquire().expect("second permit");
        // Both slots are held by in-flight work; shrinking to 0 must not
        // touch either of them.
        admission.reconcile(0).await;
        drop(a);
        drop(b);
        // Both permits were in flight when the limit dropped to 0, so
        // neither drop should have restored capacity to the pool.
        assert!(
            admission.try_acquire().is_none(),
            "a shrink queued against in-flight permits must still be honored \
             once each of them is released, not forgotten"
        );
    }

    #[tokio::test]
    async fn shrinking_reclaims_idle_capacity_immediately() {
        let admission = Admission::new(3);
        let _a = admission.try_acquire().expect("one permit held");
        // Two of the three slots are idle right now.
        admission.reconcile(1).await;
        // The one held permit is still the only thing admitted; the two idle
        // slots were reclaimed at once rather than waiting for a release that
        // was never going to happen.
        assert!(admission.try_acquire().is_none());
    }

    #[tokio::test]
    async fn growing_back_up_cancels_a_still_pending_shrink_debt() {
        let admission = Admission::new(2);
        let a = admission.try_acquire().expect("first permit");
        let b = admission.try_acquire().expect("second permit");
        // Both held: shrinking to 0 can reclaim nothing idle, so the whole
        // shrink becomes pending debt.
        admission.reconcile(0).await;
        // Change of mind, back to the original limit, before either permit
        // is released.
        admission.reconcile(2).await;
        drop(a);
        drop(b);
        // If growing had not cancelled the debt, these two releases would
        // have been silently forgotten instead of returned, leaving the gate
        // permanently short two permits relative to the limit it now claims.
        let c = admission.try_acquire();
        let d = admission.try_acquire();
        assert!(c.is_some() && d.is_some(), "growing back to 2 must restore both permits");
        assert!(admission.try_acquire().is_none(), "still exactly 2, not more");
    }

    #[tokio::test]
    async fn a_declined_acquire_never_consumes_capacity() {
        let admission = Admission::new(1);
        let _a = admission.try_acquire().expect("first permit");
        assert!(admission.try_acquire().is_none());
        assert!(admission.try_acquire().is_none(), "declining twice must not double-charge anything");
    }
}
