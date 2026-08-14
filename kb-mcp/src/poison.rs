//! Recovering from a poisoned mutex instead of inheriting the panic (BU-18).
//!
//! A `std::sync::Mutex` is poisoned when a thread panics while holding it, and
//! every later `lock()` returns `Err` for the rest of the process's life. The
//! server used `.unwrap()` on all of them, so **one** panic anywhere under a
//! lock turned every subsequent request that needed it into a panic — the
//! process stays up, answers nothing, and never recovers. `run_blocking`'s
//! `JoinError` arm turns each of those into an error response, which is exactly
//! what makes the state so quiet: an operator sees a live daemon returning
//! "internal error" forever, with the original panic scrolled far out of sight.
//!
//! Nothing here makes a panic less bad. It decides what the *next* request sees.
//!
//! ## What recovery means for each payload
//!
//! - **Plain data** (`Option<IndexingState>`, the admission counter in
//!   `transport::http`): unconditionally safe. The worst a panic can leave
//!   behind is a stale count, and a stale count beats a permanently broken one.
//!   The indexing guard is the sharper case: it used to skip its decrement on a
//!   poisoned lock, which pins `/api/admin/status` at `indexing.active=true`
//!   forever.
//! - **`Database`** (a rusqlite `Connection`): safe *after* a check. Unwinding
//!   past an open `Transaction` runs its `Drop`, which issues the `ROLLBACK`, so
//!   the usual panic leaves nothing behind. What is left behind is the case
//!   where **that rollback failed**: rusqlite swallows an error in `Drop`, and
//!   the connection is not dropped either, because the mutex outlives the
//!   panicking thread. Then the transaction is still open, every later write
//!   joins it silently (the `&self` API is autocommit-aware by design), and
//!   nobody commits it. [`recover_db`] asks the connection whether that
//!   happened and rolls back explicitly if so — cheap, and the only branch that
//!   is not "do nothing" says so in the log.
//! - **`Embedder` / `Reranker`** (fastembed over ONNX Runtime): recovery is a
//!   judgement, not a proof. Neither crate exposes a way to ask a session
//!   whether it is still consistent, so "the session is fine" cannot be
//!   verified from here — the alternative (drop it and reload the model) costs
//!   a multi-second reload and ~130 MB–2.3 GB of work on a path that has never
//!   fired. Inference does not mutate the session; the `&mut` is for the
//!   tokenizer and batching buffers, which are rebuilt per call. We take the
//!   recovery and say plainly that it is a bet.
//!
//! Reachability today is low: the code under these locks propagates with `?`
//! and has no `unwrap`. That is the point — this is about the panic someone
//! adds later, which would otherwise take the whole server's search with it.

use crate::db::Database;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LockResult, MutexGuard, TryLockError, TryLockResult};

/// Whether a poisoned lock has already been reported in this process.
///
/// The first recovery is a `warn!` naming the mutex; the rest are `debug!`.
/// Poisoning is sticky, so every later request would repeat the same line —
/// BU-32 produced 1744 warn lines from a one-second probe by logging a sticky
/// condition per request, and the fix there was the same shape as here.
static POISON_REPORTED: AtomicBool = AtomicBool::new(false);

/// Returns whether this was the first report in the process (= the loud one).
fn report(label: &str) -> bool {
    if POISON_REPORTED.swap(true, Ordering::Relaxed) {
        tracing::debug!(mutex = label, "recovered from a poisoned mutex again");
        false
    } else {
        tracing::warn!(
            mutex = label,
            "a mutex was poisoned by an earlier panic; continuing with the state it \
             left behind. Search keeps working, but something panicked — look for the \
             panic above this line."
        );
        true
    }
}

/// Take the guard, recovering the value if an earlier panic poisoned the lock.
///
/// Call it as `recover(self.thing.lock(), "thing")`: taking the `LockResult`
/// rather than the mutex keeps the literal `.lock()` at the call site, which
/// `server::tests::tool_handlers_do_not_block_the_runtime` scans for.
pub(crate) fn recover<'a, T>(
    result: LockResult<MutexGuard<'a, T>>,
    label: &str,
) -> MutexGuard<'a, T> {
    match result {
        Ok(guard) => guard,
        Err(poisoned) => {
            report(label);
            poisoned.into_inner()
        }
    }
}

/// [`recover`] for `try_lock`: `None` means *busy*, never *poisoned*.
///
/// The two are the same `Err` to a caller that only writes `.ok()`, so a
/// poisoned lock degrades a best-effort read into a permanent one — admin
/// status would report `documents: null` forever and look like contention.
pub(crate) fn recover_try<'a, T>(
    result: TryLockResult<MutexGuard<'a, T>>,
    label: &str,
) -> Option<MutexGuard<'a, T>> {
    match result {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => {
            report(label);
            Some(poisoned.into_inner())
        }
        Err(TryLockError::WouldBlock) => None,
    }
}

/// [`recover`] for the database, rolling back a transaction the panic left open.
///
/// See the module docs for why this repair is needed and why rusqlite's `Drop`
/// does not cover it.
pub(crate) fn recover_db(result: LockResult<MutexGuard<'_, Database>>) -> MutexGuard<'_, Database> {
    match result {
        Ok(guard) => guard,
        Err(poisoned) => {
            report("db");
            let guard = poisoned.into_inner();
            repair(&guard);
            guard
        }
    }
}

/// [`recover_try`] for the database, with the same repair.
pub(crate) fn recover_db_try(
    result: TryLockResult<MutexGuard<'_, Database>>,
) -> Option<MutexGuard<'_, Database>> {
    match result {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(poisoned)) => {
            report("db");
            let guard = poisoned.into_inner();
            repair(&guard);
            Some(guard)
        }
        Err(TryLockError::WouldBlock) => None,
    }
}

fn repair(db: &Database) {
    match db.rollback_if_transaction_open() {
        Ok(true) => tracing::warn!(
            "rolled back a transaction the panic left open; writes made inside it are gone"
        ),
        Ok(false) => {}
        Err(e) => tracing::error!(
            error = %e,
            "a transaction was left open by a panic and could not be rolled back; \
             writes from here on may be joining it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Poison `mutex` the way production would: a thread panics while holding it.
    fn poison<T: Send + 'static>(mutex: &Arc<Mutex<T>>) {
        let handle = {
            let mutex = Arc::clone(mutex);
            std::thread::spawn(move || {
                let _guard = mutex.lock().unwrap();
                panic!("poisoning the mutex on purpose");
            })
        };
        assert!(
            handle.join().is_err(),
            "the helper thread was supposed to panic"
        );
        assert!(mutex.is_poisoned(), "the mutex should now be poisoned");
    }

    #[test]
    fn a_poisoned_lock_still_hands_back_the_value() {
        let counter = Arc::new(Mutex::new(7_u32));
        poison(&counter);

        assert!(
            counter.lock().is_err(),
            "precondition: the plain lock() fails, which is what unwrap() used to panic on"
        );
        let mut guard = recover(counter.lock(), "counter");
        assert_eq!(*guard, 7, "the value the panic left behind must survive");
        *guard += 1;
        drop(guard);

        assert_eq!(
            *recover(counter.lock(), "counter"),
            8,
            "the mutex must stay usable, not recover once and then break again"
        );
    }

    #[test]
    fn try_lock_separates_a_poisoned_lock_from_a_busy_one() {
        let value = Arc::new(Mutex::new(3_u32));
        poison(&value);
        assert_eq!(
            recover_try(value.try_lock(), "value").map(|g| *g),
            Some(3),
            "a poisoned try_lock must yield the value, not look like contention"
        );

        let busy = Arc::new(Mutex::new(3_u32));
        let held = busy.lock().unwrap();
        assert!(
            recover_try(busy.try_lock(), "value").is_none(),
            "a genuinely held lock must still report unavailable"
        );
        drop(held);
    }

    /// Loud once, quiet after. Poisoning is sticky, so a per-recovery `warn!`
    /// would print on every request for the rest of the process's life.
    #[test]
    fn only_the_first_report_is_the_loud_one() {
        POISON_REPORTED.store(false, Ordering::Relaxed);
        assert!(report("first"), "the first poisoned lock must be loud");
        assert!(
            !report("second"),
            "a sticky condition must not keep writing warn lines"
        );
        assert!(!report("third"));
    }
}
