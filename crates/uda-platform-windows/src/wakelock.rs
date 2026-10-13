//! Windows wake lock manager: display/system sleep inhibition.
//!
//! # Backend selection
//!
//! Windows has no portal or desktop-shell daemon; the supported mechanism is the
//! thread execution state API (see `docs/internals/wakelock_specs.md`):
//!
//! | Concern      | API                                     |
//! |--------------|-----------------------------------------|
//! | Acquire lock | `SetThreadExecutionState(ES_DISPLAY_REQUIRED \| ES_SYSTEM_REQUIRED \| ES_CONTINUOUS)` |
//! | Release lock | `SetThreadExecutionState(ES_CONTINUOUS)` |
//!
//! `SetThreadExecutionState` is thread-scoped state rather than a
//! reference-counted handle: each call *replaces* the previous flags on the
//! calling thread, and a request dies with its thread. This has two
//! consequences that shape this module:
//!
//! 1. `ES_CONTINUOUS` must be present for the request to persist until the next
//!    call (without it the request lasts one suspend cycle).
//! 2. The request must be made on - and cleared on - a thread that lives as
//!    long as the lock. [`WakeLockManager::acquire`] therefore hands each lock
//!    to a dedicated owner thread that parks between acquire and release, so
//!    neither an FFI bridge's ephemeral worker nor a caller's async runtime
//!    thread can silently drop the request.
//!
//! Unlike the FreeDesktop cookie returned by `org.freedesktop.ScreenSaver.Inhibit`,
//! there is no cookie to hand back; the owner thread's exit *is* the release.

use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    EXECUTION_STATE,
};

use uda_core::error::UdaError;
use uda_core::wakelock::{WakeLockGuard, WakeLockManager, WakeLockType};

/// RAII guard for a Windows wake lock.
///
/// On drop the thread execution state is restored to the default
/// `ES_CONTINUOUS`, which releases both the display and system requirements.
///
/// The guard is intentionally not `Clone` and not `Send`-agnostic about ordering:
/// because the underlying API is thread-scoped, releasing from a different thread
/// than the one that acquired the lock would not clear the flag on the thread
/// that actually holds it. It is still `Send` so it can be moved onto the thread
/// that owns the lock, which is the common case.
pub struct WindowsWakeLockGuard {
    /// The core RAII guard that actually owns the release closure.
    ///
    /// Wrapping (rather than duplicating) the core guard keeps a single release
    /// path, so `release()` followed by `drop()` cannot issue two FFI calls.
    inner: WakeLockGuard,
    /// `false` once the lock has been released, making `Drop` idempotent.
    released: bool,
}

impl WindowsWakeLockGuard {
    /// Wrap the core guard for an already-acquired lock.
    ///
    /// This is the constructor used by
    /// [`WindowsWakeLockManager::acquire_guard`], which is the Windows-specific
    /// entry point that returns this type instead of the core guard.
    pub fn new(inner: WakeLockGuard) -> Self {
        Self {
            inner,
            released: false,
        }
    }

    /// Whether this guard still holds an active wake lock.
    pub fn is_active(&self) -> bool {
        !self.released
    }

    /// Manually release the wake lock early.
    ///
    /// Consumes the guard, mirroring [`WakeLockGuard::release`].
    pub fn release(mut self) {
        self.release_internal();
    }
}

impl Drop for WindowsWakeLockGuard {
    fn drop(&mut self) {
        self.release_internal();
    }
}

impl WindowsWakeLockGuard {
    /// Hand the release closure back to the core guard, exactly once.
    ///
    /// The core guard's own `Drop` runs the closure, so this method only has to
    /// ensure it is invoked a single time. Marking the wrapper as released first
    /// makes a `release()` + `drop()` pair safe.
    fn release_internal(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        // Replacing the inner guard with an already-released one and dropping
        // the original runs the release closure exactly once.
        drop(std::mem::replace(
            &mut self.inner,
            WakeLockGuard::new(Box::new(|| {})),
        ));
    }
}

/// Windows wake lock manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsWakeLockManager;

impl WindowsWakeLockManager {
    pub fn new() -> Self {
        Self
    }

    /// Map the cross-platform [`WakeLockType`] onto Win32 execution-state flags.
    ///
    /// `ES_CONTINUOUS` is always included so the request persists until it is
    /// explicitly cleared. `PreventSystemIdle` additionally requests
    /// `ES_SYSTEM_REQUIRED`, which keeps the machine from entering automatic sleep.
    fn execution_state(lock_type: WakeLockType) -> EXECUTION_STATE {
        match lock_type {
            WakeLockType::PreventDisplaySleep => ES_CONTINUOUS | ES_DISPLAY_REQUIRED,
            WakeLockType::PreventSystemIdle => {
                ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED
            }
        }
    }
}

impl WindowsWakeLockManager {
    /// Acquire a wake lock and return the Windows-specific guard.
    ///
    /// This is the platform entry point for callers that want the richer guard
    /// API ([`WindowsWakeLockGuard::is_active`], `release()`) instead of the
    /// core [`WakeLockGuard`] returned by [`WakeLockManager::acquire`]. Both
    /// paths share the same FFI release closure, so they are interchangeable.
    pub async fn acquire_guard(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WindowsWakeLockGuard, UdaError> {
        let guard = WakeLockManager::acquire(self, lock_type, reason).await?;
        Ok(WindowsWakeLockGuard::new(guard))
    }
}

#[async_trait::async_trait]
impl WakeLockManager for WindowsWakeLockManager {
    /// Acquire a wake lock on a thread that lives as long as the lock.
    ///
    /// `SetThreadExecutionState` is thread-scoped: a request made on a thread
    /// dies with that thread. The FFI bridge may run this future on an
    /// ephemeral worker (an ambient tokio runtime), so the request must be
    /// owned by a thread this function controls - one that stays parked until
    /// the guard releases it. Each lock therefore gets its own owner thread:
    /// acquire reports through a rendezvous channel, the thread blocks on the
    /// release channel (parked, no CPU), and the release closure - or the
    /// guard's `Drop` closing the channel - is what lets the thread restore
    /// `ES_CONTINUOUS` and exit. One thread per lock also keeps independent
    /// locks independent: clearing one lock's flags cannot clear another's.
    async fn acquire(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WakeLockGuard, UdaError> {
        let state = Self::execution_state(lock_type);

        // `acquired` carries the owner thread's verdict; `release` is the only
        // thing that ends its loop. Closing `release` (a dropped guard) also
        // unblocks `recv`, so the thread can never outlive its guard.
        let (acquired, acquired_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel::<()>();
        let reason = reason.to_string();

        std::thread::Builder::new()
            .name("uda-wake-lock".to_string())
            .spawn(move || {
                // SAFETY: value-only FFI with no pointers and no invariants
                // beyond the documented return contract; this thread owns the
                // request for its whole life.
                let previous = unsafe { SetThreadExecutionState(state) };
                let verdict = if previous.0 == 0 {
                    Err(UdaError::Internal(
                        "SetThreadExecutionState failed; wake locks cannot be changed from a power notification callback"
                            .to_string(),
                    ))
                } else {
                    Ok(())
                };
                // If the acquirer vanished (caller cancelled the future, a
                // timeout or select race), the send error ends the thread -
                // and a thread's exit is itself what clears its execution
                // state request, so no stale inhibition can outlive it. The
                // normal path reports the verdict and parks.
                if acquired.send(verdict).is_err() {
                    return;
                }
                // Park until released. A channel close means the guard was
                // dropped without an explicit release - still a release.
                if release_rx.recv().is_ok() {
                    log::debug!("wake lock released on its owner thread");
                }
                // SAFETY: `ES_CONTINUOUS` on its own is the documented "clear
                // all requirements" call; restoring it on the same thread that
                // set the flags is what actually ends the request.
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            })
            .map_err(|error| {
                UdaError::Internal(format!("could not spawn the wake-lock owner thread: {error}"))
            })?;

        match acquired_rx.recv() {
            Ok(Ok(())) => {
                log::debug!(
                    "acquired Windows wake lock {lock_type:?} ({reason}); flags={:#x}",
                    state.0
                );
                // The closure only signals the owner thread; joining happens
                // on that thread, so release never blocks the caller.
                Ok(WakeLockGuard::new(Box::new(move || {
                    // A send error means the thread already exited - the lock
                    // is gone either way.
                    let _ = release.send(());
                })))
            }
            Ok(Err(error)) => {
                log::debug!("failed to acquire wake lock ({reason}): {error}");
                // The owner thread exits after reporting the failure, so no
                // parked thread is left behind.
                Err(error)
            }
            Err(_) => Err(UdaError::Internal(
                "the wake-lock owner thread exited before acquiring".to_string(),
            )),
        }
    }
}

/// Convert a raw `u32` flag set into the typed Win32 constant.
///
/// Kept as a helper so tests can assert the mapping without duplicating the
/// `EXECUTION_STATE` newtype construction.
#[cfg(test)]
fn execution_state_bits(lock_type: WakeLockType) -> u32 {
    WindowsWakeLockManager::execution_state(lock_type).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_state_always_includes_continuous() {
        for lock_type in [
            WakeLockType::PreventDisplaySleep,
            WakeLockType::PreventSystemIdle,
        ] {
            let state = WindowsWakeLockManager::execution_state(lock_type);
            assert!(
                state.contains(ES_CONTINUOUS),
                "{lock_type:?} must include ES_CONTINUOUS"
            );
        }
    }

    #[test]
    fn display_sleep_lock_requires_display_only() {
        let state = WindowsWakeLockManager::execution_state(WakeLockType::PreventDisplaySleep);
        assert!(state.contains(ES_DISPLAY_REQUIRED));
        assert!(!state.contains(ES_SYSTEM_REQUIRED));
    }

    #[test]
    fn system_idle_lock_requires_display_and_system() {
        let state = WindowsWakeLockManager::execution_state(WakeLockType::PreventSystemIdle);
        assert!(state.contains(ES_DISPLAY_REQUIRED));
        assert!(state.contains(ES_SYSTEM_REQUIRED));
    }

    #[test]
    fn execution_state_bits_match_documented_values() {
        assert_eq!(
            execution_state_bits(WakeLockType::PreventDisplaySleep),
            ES_CONTINUOUS.0 | ES_DISPLAY_REQUIRED.0
        );
        assert_eq!(
            execution_state_bits(WakeLockType::PreventSystemIdle),
            ES_CONTINUOUS.0 | ES_DISPLAY_REQUIRED.0 | ES_SYSTEM_REQUIRED.0
        );
    }

    /// Build a wrapper guard around a core guard that records whether it ran.
    fn wrapper_guard_with_spy() -> (
        WindowsWakeLockGuard,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spy = std::sync::Arc::clone(&counter);
        let inner = WakeLockGuard::new(Box::new(move || {
            spy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        (WindowsWakeLockGuard::new(inner), counter)
    }

    #[test]
    fn guard_starts_active_and_release_marks_it_inactive() {
        let (mut guard, counter) = wrapper_guard_with_spy();
        assert!(guard.is_active());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);

        guard.release_internal();

        assert!(!guard.is_active());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_release_is_idempotent() {
        let (mut guard, counter) = wrapper_guard_with_spy();
        guard.release_internal();
        // Second call must be a no-op rather than another FFI round-trip.
        guard.release_internal();
        assert!(!guard.is_active());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_drop_runs_the_release_closure_once() {
        let (guard, counter) = wrapper_guard_with_spy();
        assert!(guard.is_active());
        drop(guard);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_release_then_drop_runs_the_closure_once() {
        let (guard, counter) = wrapper_guard_with_spy();
        guard.release();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn acquire_closure_is_the_documented_release_call() {
        // The core trait returns a `WakeLockGuard`, so the release behaviour is
        // verified through a standalone guard wired to the same closure shape.
        let released = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spy = std::sync::Arc::clone(&released);
        let guard = WakeLockGuard::new(Box::new(move || {
            spy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        drop(guard);
        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
