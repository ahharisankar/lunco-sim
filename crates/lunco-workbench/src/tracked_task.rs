//! Tracked async work — couples a Bevy `Task` to a [`StatusBus`]
//! [`BusyHandle`] so any work that produces user-visible state is
//! represented on the bus from spawn to completion automatically.
//!
//! The handle is moved into the future, so it drops the moment the
//! future resolves (whether to a value, a panic, or because the
//! caller dropped the `Task` to cancel). UI overlays that read
//! `bus.is_busy(scope)` see continuous "busy" without per-call-site
//! bookkeeping.
//!
//! Prefer this over bare `AsyncComputeTaskPool::spawn` whenever a
//! panel's loading indicator should reflect the work.

use std::future::Future;

use bevy::tasks::{AsyncComputeTaskPool, Task};

use crate::status_bus::{BusyScope, StatusBus};

/// Wrapper around [`bevy::tasks::Task`] whose lifetime is tied to a
/// [`BusyHandle`] held inside the spawned future. Dropping the
/// `TrackedTask` cancels (Bevy drops the future, the handle drops,
/// bus clears the entry on next `drain_busy_drops`).
pub struct TrackedTask<T> {
    inner: Task<T>,
}

impl<T> TrackedTask<T> {
    /// Poll the inner task without blocking. Mirrors the existing
    /// `futures_lite::future::poll_once` pattern used throughout the
    /// codebase.
    pub fn poll_once(&mut self) -> Option<T> {
        use bevy::tasks::futures_lite::future;
        future::block_on(future::poll_once(&mut self.inner))
    }

    /// Borrow the inner `Task` for callers that need the raw type
    /// (e.g. for `cancel`, `is_finished`, or direct polling).
    pub fn inner_mut(&mut self) -> &mut Task<T> {
        &mut self.inner
    }
}

/// Spawn `fut` on the async-compute pool, registering a busy entry
/// at `(scope, source)` for its entire lifetime. The handle is moved
/// into the future, so any exit path — return, panic, drop — releases
/// the bus entry.
pub fn spawn_tracked<T: Send + 'static>(
    bus: &mut StatusBus,
    scope: BusyScope,
    source: &'static str,
    label: impl Into<String>,
    fut: impl Future<Output = T> + Send + 'static,
) -> TrackedTask<T> {
    let handle = bus.begin(scope, source, label);
    let task = AsyncComputeTaskPool::get().spawn(async move {
        let out = fut.await;
        drop(handle);
        out
    });
    TrackedTask { inner: task }
}

/// [`spawn_tracked`] variant that also wires a cooperative cancel
/// token into the bus entry, surfacing a `[✕]` button on the
/// indicator. The future must observe the token at its checkpoints —
/// dropping the handle alone does not abort an already-running
/// pool task.
pub fn spawn_tracked_cancellable<T: Send + 'static>(
    bus: &mut StatusBus,
    scope: BusyScope,
    source: &'static str,
    label: impl Into<String>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    fut: impl Future<Output = T> + Send + 'static,
) -> TrackedTask<T> {
    let handle = bus.begin_cancellable(scope, source, label, cancel);
    let task = AsyncComputeTaskPool::get().spawn(async move {
        let out = fut.await;
        drop(handle);
        out
    });
    TrackedTask { inner: task }
}
