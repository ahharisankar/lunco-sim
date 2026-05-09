//! Cross-cutting status bus for the workbench.
//!
//! Every subsystem (MSL load, compile, sim, save, API, …) publishes
//! `StatusEvent`s into the [`StatusBus`] resource. A single set of
//! renderers fans events out to:
//!
//! - **Status bar** at the bottom of the viewport — latest live event
//!   per source, clickable to open a history popover.
//! - **Console panel** — every event, chronological audit trail.
//! - **Diagnostics panel** — only error/warning events tied to the
//!   active document.
//!
//! Subsystems don't know about any of those views; they just `push` and
//! the right surfaces light up. New views (egui native status bar,
//! external API stream) just subscribe to the bus.
//!
//! ## Two flavours of event
//!
//! - **Discrete** events ([`StatusBus::push`]) are appended to `history`
//!   and shown in Console / Diagnostics. Use for "MSL ready",
//!   "compile started", "save failed".
//! - **Progress** events ([`StatusBus::push_progress`]) replace the
//!   most recent progress entry from the same source instead of being
//!   appended — they would otherwise spam the history during a long
//!   download. Each `(done, total)` tick *replaces* the prior tick from
//!   that source. Once `done == total`, callers typically follow with
//!   a discrete `Info` event (e.g. "MSL ready") to terminate.
//!
//! ## Change detection
//!
//! `seq()` increments on every push. Renderers cache the last seq they
//! saw and skip the DOM/UI update when nothing moved.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use bevy::prelude::*;
use web_time::Instant;

/// Scope of a busy/progress entry. Determines which surfaces render the
/// indicator (per-tab overlay, per-node tree row, global status bar) and
/// allows aggregate queries via [`StatusBus::is_busy`].
///
/// IDs are opaque `u64` newtypes so this enum stays decoupled from the
/// concrete document/tab/node types in upstream crates. Convert at the
/// call site: `BusyScope::Tab(tab_id.0)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BusyScope {
    /// Whole application — appears in the bottom status bar.
    Global,
    /// Tied to a specific document; multi-tab views of that document
    /// can all reflect the same busy state.
    Document(u64),
    /// Tied to a specific tab/pane; only that tab's overlay renders it.
    Tab(u64),
    /// Tied to a tree node (e.g. a package-browser row).
    Node(u64),
}

impl BusyScope {
    /// Returns `true` when `self` is `other` or contained by `other` in
    /// the scope hierarchy. Used by [`StatusBus::is_busy`] to answer
    /// "is anything in this scope busy?" queries.
    ///
    /// Hierarchy (today): `Tab` ⊂ `Global`; `Node` ⊂ `Global`; `Document`
    /// ⊂ `Global`. Tab/Document linkage is resolved by the caller (the
    /// bus does not know which tab belongs to which document).
    pub fn is_within(self, other: BusyScope) -> bool {
        if self == other {
            return true;
        }
        matches!(other, BusyScope::Global)
    }
}

/// Opaque identifier for a single in-flight busy entry. Issued by
/// [`StatusBus::begin`] and carried by [`BusyHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusyId(u64);

/// RAII guard for an in-flight busy entry. Move into the task / per-tab
/// state whose lifetime defines the work; on `Drop` the bus removes the
/// entry on the next frame (via a drained mpsc channel) so callers
/// cannot leak progress state by forgetting to call `clear_progress`.
///
/// Send-safe: may be moved into `AsyncComputeTaskPool` futures.
pub struct BusyHandle {
    id: BusyId,
    drop_tx: Sender<BusyId>,
}

impl BusyHandle {
    /// Identifier for this handle. Stable for the handle's lifetime.
    pub fn id(&self) -> BusyId {
        self.id
    }
}

impl Drop for BusyHandle {
    fn drop(&mut self) {
        // Best-effort: receiver is held by the bus; if the bus has been
        // dropped (e.g. app shutdown) the send simply fails.
        let _ = self.drop_tx.send(self.id);
    }
}

/// Maximum number of *discrete* events kept in `history`. Progress
/// events don't count against this — they're stored separately.
pub const STATUS_HISTORY_CAPACITY: usize = 200;

/// Severity / classification of a status event. Drives:
/// - Status bar dot colour.
/// - Console log level.
/// - Diagnostics inclusion (Error / Warn surface there; Info / Progress don't).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusLevel {
    Info,
    Warn,
    Error,
    /// In-flight progress tick. Replaces the last `Progress` from the
    /// same source instead of appending. Use [`StatusBus::push_progress`].
    Progress,
}

/// One status event. Carries everything the renderers need without
/// extra resource look-ups.
#[derive(Debug, Clone)]
pub struct StatusEvent {
    /// Scope of this event. Discrete events default to [`BusyScope::Global`];
    /// scoped progress originates from [`StatusBus::begin`].
    pub scope: BusyScope,
    /// Short subsystem identifier shown to the user (`"MSL"`, `"Compile"`).
    pub source: &'static str,
    pub level: StatusLevel,
    pub message: String,
    /// `(done, total)` for progress events; `None` for discrete events.
    /// `total == 0` means indeterminate progress (show shimmer / spinner).
    pub progress: Option<(u64, u64)>,
    pub at: Instant,
    /// Opaque id when this event is the active progress for a [`BusyHandle`].
    /// `None` for discrete events and for legacy [`StatusBus::push_progress`].
    pub busy_id: Option<BusyId>,
}

impl StatusEvent {
    /// Percentage `0.0..=100.0` derived from `progress`. Returns `None`
    /// when the event has no progress or `total == 0`.
    pub fn progress_pct(&self) -> Option<f64> {
        let (done, total) = self.progress?;
        if total == 0 {
            return None;
        }
        Some((done as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
    }
}

/// Workbench-wide status bus. Insert via [`StatusBusPlugin`].
///
/// Carries two flavours of state — discrete history events (info / warn /
/// error) and active progress entries keyed by `(scope, source)`. Active
/// progress is what indicators read; history is what the status-bar
/// popup and toast renderer consume.
#[derive(Resource)]
pub struct StatusBus {
    /// Append-only history of *discrete* events, capped at
    /// [`STATUS_HISTORY_CAPACITY`]. Older entries fall off the front.
    history: VecDeque<StatusEvent>,
    /// Latest in-flight progress per `(scope, source)`. Replaced on every
    /// `push_progress` from the same key; cleared by `clear_progress`,
    /// `end`, or [`BusyHandle`] drop.
    active_progress: HashMap<(BusyScope, &'static str), StatusEvent>,
    /// Reverse index: which `(scope, source)` does a given `BusyId` own?
    /// Lets [`BusyHandle::Drop`] clear the right entry without the caller
    /// remembering its own scope/source.
    by_id: HashMap<BusyId, (BusyScope, &'static str)>,
    /// Bumped on every push (discrete or progress). Renderers cache the
    /// last seq they saw to skip work when the bus hasn't changed.
    seq: u64,
    /// Monotonic counter for new [`BusyId`]s.
    next_id: u64,
    /// Sender cloned into every [`BusyHandle`]. The matching receiver
    /// lives in `drop_rx`; `drain_busy_drops` walks it each frame.
    drop_tx: Sender<BusyId>,
    /// Receiver for handle-drop notifications. `Mutex` because Bevy
    /// requires `Resource: Send + Sync` and `Receiver` is `!Sync`.
    drop_rx: Mutex<Receiver<BusyId>>,
}

impl Default for StatusBus {
    fn default() -> Self {
        let (drop_tx, drop_rx) = channel();
        Self {
            history: VecDeque::new(),
            active_progress: HashMap::new(),
            by_id: HashMap::new(),
            seq: 0,
            next_id: 0,
            drop_tx,
            drop_rx: Mutex::new(drop_rx),
        }
    }
}

impl StatusBus {
    /// Append a discrete event to history.
    pub fn push(
        &mut self,
        source: &'static str,
        level: StatusLevel,
        message: impl Into<String>,
    ) {
        debug_assert!(
            level != StatusLevel::Progress,
            "use push_progress for Progress events"
        );
        let ev = StatusEvent {
            scope: BusyScope::Global,
            source,
            level,
            message: message.into(),
            progress: None,
            at: Instant::now(),
            busy_id: None,
        };
        if self.history.len() >= STATUS_HISTORY_CAPACITY {
            self.history.pop_front();
        }
        self.history.push_back(ev);
        self.seq = self.seq.wrapping_add(1);
    }

    /// Update / install the active progress tick for `source`. Does
    /// not append to `history` — call `push(..., Info, ...)` separately
    /// to mark phase transitions you want preserved.
    pub fn push_progress(
        &mut self,
        source: &'static str,
        message: impl Into<String>,
        done: u64,
        total: u64,
    ) {
        let ev = StatusEvent {
            scope: BusyScope::Global,
            source,
            level: StatusLevel::Progress,
            message: message.into(),
            progress: Some((done, total)),
            at: Instant::now(),
            busy_id: None,
        };
        self.active_progress.insert((BusyScope::Global, source), ev);
        self.seq = self.seq.wrapping_add(1);
    }

    /// Drop the active progress tick for `(BusyScope::Global, source)`.
    /// Legacy entry point — prefer [`BusyHandle`] drop for new code.
    pub fn clear_progress(&mut self, source: &'static str) {
        if self
            .active_progress
            .remove(&(BusyScope::Global, source))
            .is_some()
        {
            self.seq = self.seq.wrapping_add(1);
        }
    }

    /// Begin tracking an in-flight unit of work. Returns a [`BusyHandle`]
    /// whose `Drop` removes the entry on the next frame (via
    /// `drain_busy_drops`). Move the handle into the future / per-tab
    /// state whose lifetime defines the work.
    ///
    /// Replaces any existing entry at `(scope, source)` — this is the
    /// same dedup behaviour as [`Self::push_progress`], extended to scopes.
    pub fn begin(
        &mut self,
        scope: BusyScope,
        source: &'static str,
        label: impl Into<String>,
    ) -> BusyHandle {
        let id = BusyId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        // Evict any prior entry at the same (scope, source) — keep the
        // by_id index in sync so the prior handle's drop is a no-op.
        if let Some(prev) = self.active_progress.get(&(scope, source)) {
            if let Some(prev_id) = prev.busy_id {
                self.by_id.remove(&prev_id);
            }
        }
        let ev = StatusEvent {
            scope,
            source,
            level: StatusLevel::Progress,
            message: label.into(),
            progress: None,
            at: Instant::now(),
            busy_id: Some(id),
        };
        self.active_progress.insert((scope, source), ev);
        self.by_id.insert(id, (scope, source));
        self.seq = self.seq.wrapping_add(1);
        BusyHandle {
            id,
            drop_tx: self.drop_tx.clone(),
        }
    }

    /// Update the progress tick for an outstanding [`BusyHandle`].
    /// `total == 0` means indeterminate.
    pub fn with_progress(&mut self, handle: &BusyHandle, done: u64, total: u64) {
        let Some(&(scope, source)) = self.by_id.get(&handle.id) else {
            return;
        };
        let Some(ev) = self.active_progress.get_mut(&(scope, source)) else {
            return;
        };
        ev.progress = Some((done, total));
        ev.at = Instant::now();
        self.seq = self.seq.wrapping_add(1);
    }

    /// Update the human-readable label for an outstanding [`BusyHandle`].
    pub fn with_label(&mut self, handle: &BusyHandle, label: impl Into<String>) {
        let Some(&(scope, source)) = self.by_id.get(&handle.id) else {
            return;
        };
        let Some(ev) = self.active_progress.get_mut(&(scope, source)) else {
            return;
        };
        ev.message = label.into();
        ev.at = Instant::now();
        self.seq = self.seq.wrapping_add(1);
    }

    /// Internal: clear the entry owned by `id`. Called by
    /// `drain_busy_drops` when a [`BusyHandle`] is dropped.
    fn clear_by_id(&mut self, id: BusyId) {
        let Some(key) = self.by_id.remove(&id) else {
            return;
        };
        // Only clear if this id still owns the slot — a re-`begin` at
        // the same key would have already evicted us via `by_id`.
        if let Some(ev) = self.active_progress.get(&key) {
            if ev.busy_id == Some(id) {
                self.active_progress.remove(&key);
                self.seq = self.seq.wrapping_add(1);
            }
        }
    }

    /// `true` if any active progress entry's scope is within `scope`.
    /// Walks the parent-of relation — `is_busy(BusyScope::Global)` is
    /// `true` whenever anything is busy.
    pub fn is_busy(&self, scope: BusyScope) -> bool {
        self.active_progress
            .keys()
            .any(|(s, _)| s.is_within(scope))
    }

    /// Iterator over active entries whose scope is within `scope`.
    pub fn entries_in(&self, scope: BusyScope) -> impl Iterator<Item = &StatusEvent> {
        self.active_progress
            .iter()
            .filter_map(move |((s, _), ev)| s.is_within(scope).then_some(ev))
    }

    /// Longest-running active entry within `scope`, useful for the
    /// "show one indicator" rendering case.
    pub fn longest_in(&self, scope: BusyScope) -> Option<&StatusEvent> {
        self.entries_in(scope).min_by_key(|ev| ev.at)
    }

    /// Iterator over the discrete history, oldest first.
    /// Double-ended so callers can render newest-first via `.rev()`.
    pub fn history(&self) -> std::collections::vec_deque::Iter<'_, StatusEvent> {
        self.history.iter()
    }

    /// Iterator over the active progress events. Order is unspecified;
    /// callers that show only one are expected to pick by recency.
    pub fn active_progress(&self) -> impl Iterator<Item = &StatusEvent> {
        self.active_progress.values()
    }

    /// Latest event to *display* — the most recent active progress
    /// entry if any, else the most recent discrete history entry.
    /// What renderers should show in a single-line status strip.
    pub fn display_latest(&self) -> Option<&StatusEvent> {
        self.active_progress
            .values()
            .max_by_key(|e| e.at)
            .or_else(|| self.history.back())
    }

    /// Sequence number bumped on each push. Renderers use this for
    /// cheap change-detection — store it in a `Local`, compare next
    /// frame, skip work if unchanged.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Drains [`BusyHandle`] drop notifications and clears the corresponding
/// active-progress entries. Runs every frame as part of [`StatusBusPlugin`].
pub fn drain_busy_drops(mut bus: ResMut<StatusBus>) {
    // Pull all pending ids out under the mutex first so we can release
    // it before mutating self via `clear_by_id`.
    let mut drops: Vec<BusyId> = Vec::new();
    if let Ok(rx) = bus.drop_rx.lock() {
        while let Ok(id) = rx.try_recv() {
            drops.push(id);
        }
    }
    for id in drops {
        bus.clear_by_id(id);
    }
}

/// Adds the [`StatusBus`] resource and the per-frame `drain_busy_drops`
/// system. Renderers and fan-out systems are added by their owning
/// plugins (each can opt in independently).
pub struct StatusBusPlugin;

impl Plugin for StatusBusPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StatusBus>()
            .add_systems(Update, drain_busy_drops);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_push_progress_targets_global_scope() {
        let mut bus = StatusBus::default();
        bus.push_progress("MSL", "loading", 1, 10);
        assert!(bus.is_busy(BusyScope::Global));
        assert_eq!(bus.entries_in(BusyScope::Global).count(), 1);
        bus.clear_progress("MSL");
        assert!(!bus.is_busy(BusyScope::Global));
    }

    #[test]
    fn begin_and_drop_clears_via_drain() {
        let mut bus = StatusBus::default();
        let handle = bus.begin(BusyScope::Tab(7), "drill-in", "Loading…");
        assert!(bus.is_busy(BusyScope::Tab(7)));
        assert!(bus.is_busy(BusyScope::Global));
        drop(handle);
        // Simulate a frame.
        let drops: Vec<BusyId> = bus.drop_rx.lock().unwrap().try_iter().collect();
        for id in drops {
            bus.clear_by_id(id);
        }
        assert!(!bus.is_busy(BusyScope::Tab(7)));
        assert!(!bus.is_busy(BusyScope::Global));
    }

    #[test]
    fn re_begin_evicts_prior_handle_silently() {
        let mut bus = StatusBus::default();
        let h1 = bus.begin(BusyScope::Tab(1), "drill-in", "first");
        let id1 = h1.id();
        let _h2 = bus.begin(BusyScope::Tab(1), "drill-in", "second");
        // h1's id no longer owns the slot; dropping it must not clear
        // the entry now belonging to h2.
        drop(h1);
        bus.clear_by_id(id1);
        assert!(bus.is_busy(BusyScope::Tab(1)));
    }

    #[test]
    fn distinct_scopes_with_same_source_dont_trample() {
        let mut bus = StatusBus::default();
        let _a = bus.begin(BusyScope::Tab(1), "drill-in", "tab 1");
        let _b = bus.begin(BusyScope::Tab(2), "drill-in", "tab 2");
        assert_eq!(bus.entries_in(BusyScope::Global).count(), 2);
        assert!(bus.is_busy(BusyScope::Tab(1)));
        assert!(bus.is_busy(BusyScope::Tab(2)));
    }

    #[test]
    fn with_progress_updates_existing_entry() {
        let mut bus = StatusBus::default();
        let h = bus.begin(BusyScope::Global, "compile", "compiling");
        bus.with_progress(&h, 3, 10);
        let ev = bus.longest_in(BusyScope::Global).expect("entry");
        assert_eq!(ev.progress, Some((3, 10)));
    }
}
