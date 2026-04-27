//! Event-driven file picker plumbing.
//!
//! The pattern: panels and commands fire [`PickHandle`]; a backend
//! observer (native rfd today, web File-System-Access tomorrow) resolves
//! the dialog asynchronously and dispatches a typed follow-up command
//! on the user's choice. Cancellation = silent; no follow-up fires.
//!
//! This split keeps UI code synchronous (no `async`, no polling), keeps
//! the backend swap a `cfg`-gated observer rather than a call-site
//! rewrite, and gives HTTP / scripting callers a uniform shape: trigger
//! the resolved follow-up command directly to skip the dialog, or
//! trigger [`PickHandle`] to force one.
//!
//! Event and observer wiring land in later commits; this module only
//! defines the message types so downstream crates can start emitting
//! them.

use bevy::prelude::*;

use crate::{OpenFilter, SaveHint};

// `DocumentId` lives in `lunco-doc`, a crate higher in the dependency
// graph than this one. The `SaveAs` variant therefore carries the raw
// `u64` produced by `DocumentId::raw()`; the workbench-side observer
// reconstructs the typed id at the boundary. This is the **only**
// place a u64 doc-id shim is acceptable — and only because reversing
// the dep direction (lunco-storage depending on lunco-doc) would be
// worse. See `AGENTS.md` § 4.2 anti-patterns: that rule targets
// `#[Command]` *fields*, not local Event payloads at a layer crossing.

/// What kind of system dialog to show.
#[derive(Clone, Debug)]
pub enum PickMode {
    /// "Open File" picker with a file-type filter.
    OpenFile(OpenFilter),
    /// "Save As" picker with a starting directory + suggested name.
    SaveFile(SaveHint),
    /// "Open Folder" picker (no filter).
    OpenFolder,
}

/// Which command to trigger once the picker resolves with a chosen
/// handle. A user cancellation produces no command — the in-flight
/// entity is despawned and nothing happens.
///
/// Closed enum (rather than a boxed `dyn Command`) because:
/// - every file-workflow path is enumerable; new follow-ups land here
///   intentionally rather than implicitly,
/// - the variant is `Reflect`-friendly so `PickHandle` can travel over
///   the HTTP wire if a remote agent wants to force a dialog,
/// - no `Send`/lifetime gymnastics for type-erased commands.
#[derive(Clone, Debug)]
pub enum PickFollowUp {
    /// Resolve → trigger `OpenFile { path }`.
    OpenFile,
    /// Resolve → trigger `OpenFolder { path }` (folder may or may not
    /// contain a `twin.toml`; the observer classifies and routes).
    OpenFolder,
    /// Resolve → trigger `OpenTwin { path }` (strict: errors if the
    /// chosen folder lacks a `twin.toml`).
    OpenTwin,
    /// Resolve → trigger `SaveAsDocument { doc, path }` for the doc
    /// whose id is carried here.
    SaveAs(u64),
    /// Resolve → trigger `SaveAsTwin { folder }` to promote the
    /// current session into a Twin at the chosen folder.
    SaveAsTwin,
}

/// Request to show a system file dialog.
///
/// Fired by panels, menu items, keybind resolvers, or HTTP callers.
/// Resolved asynchronously by a backend observer; on success the
/// observer triggers the [`PickFollowUp`] command with the chosen
/// handle's path filled in.
#[derive(Event, Clone, Debug)]
pub struct PickHandle {
    /// Which dialog to show.
    pub mode: PickMode,
    /// What to do with the result.
    pub on_resolved: PickFollowUp,
}

/// Marker component on the transient entity that owns an in-flight
/// picker task.
///
/// The native backend spawns one of these per active picker. The poll
/// system reads completion and despawns the entity, then triggers the
/// follow-up command. Multiple pickers can coexist (rare, but cheap to
/// allow — e.g. a dialog opens while a Save-As is already showing).
///
/// The actual task handle is backend-specific and lives on the same
/// entity as a sibling component the backend defines. This marker is
/// the cross-backend anchor that the poll system queries.
#[derive(Component)]
pub struct PickInFlight {
    /// What to dispatch on success.
    pub follow_up: PickFollowUp,
}
