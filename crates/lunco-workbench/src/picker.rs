//! Event-driven file picker.
//!
//! Storage is the wrong layer for dialog plumbing — it's the I/O
//! abstraction that loads and saves doc bytes. Picking a path is a UI
//! concern: a modal native dialog on desktop, a JS prompt on the web.
//! So the picker lives here in the workbench and just *uses*
//! `OpenFilter` / `SaveHint` / `StorageHandle` from `lunco-storage` to
//! describe the request and the result.
//!
//! ## Pattern
//!
//! Panels and commands fire [`PickHandle`]; a backend observer (native
//! `rfd` today, web File-System-Access tomorrow) resolves the dialog
//! asynchronously and emits [`PickResolved`] (success) or
//! [`PickCancelled`]. A workbench-side dispatcher reads the resolved
//! event and triggers the matching typed file-workflow command
//! (`OpenFile { path }`, `SaveAsDocument { doc, path }`, ...).
//!
//! This split keeps UI code synchronous (no `async`, no polling), keeps
//! the backend swap a `cfg`-gated observer rather than a call-site
//! rewrite, and gives HTTP / scripting callers a uniform shape: trigger
//! the resolved follow-up command directly to skip the dialog, or
//! trigger [`PickHandle`] to force one.
//!
//! Workbench-level file-workflow commands (`OpenFile`, `OpenFolder`,
//! `OpenTwin`, `SaveAsDocument`, `SaveAsTwin`) and the routing observer
//! that consumes [`PickResolved`] arrive in follow-up commits. This
//! module ships only the picker itself.

use bevy::prelude::*;
use lunco_doc::DocumentId;
use lunco_storage::{OpenFilter, SaveHint, StorageHandle};

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
/// - no `Send`/lifetime gymnastics for type-erased commands,
/// - the variant set is reviewable on every diff.
#[derive(Clone, Debug)]
pub enum PickFollowUp {
    /// Resolve → trigger `OpenFile { path }`.
    OpenFile,
    /// Resolve → trigger `OpenFolder { path }` (folder may or may not
    /// contain a `twin.toml`; the routing observer classifies and
    /// dispatches Folder vs Twin accordingly).
    OpenFolder,
    /// Resolve → trigger `OpenTwin { path }` (strict: errors if the
    /// chosen folder lacks a `twin.toml`).
    OpenTwin,
    /// Resolve → trigger `AddFolderToWorkspace { path }` (VS Code-style
    /// multi-root: keeps existing folder Twins, adds this one).
    AddFolderToWorkspace,
    /// Resolve → trigger `AddTwin { path }` (strict variant of
    /// [`Self::AddFolderToWorkspace`]; requires a `twin.toml`).
    AddTwin,
    /// Resolve → trigger `SaveAsDocument { doc, path }` for the doc
    /// whose typed id is carried here.
    SaveAs(DocumentId),
    /// Resolve → trigger `SaveAsTwin { folder }` to promote the
    /// current session into a Twin at the chosen folder.
    SaveAsTwin,
}

/// Request to show a system file dialog.
///
/// Fired by panels, menu items, keybind resolvers, or HTTP callers.
/// Resolved asynchronously by a backend observer; on success the
/// observer fires [`PickResolved`] with the chosen handle.
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
/// Multiple pickers can coexist (rare, but cheap to allow — e.g. a
/// Save-As dialog opens while an Open-File is already showing). The
/// backend-specific task handle lives as a sibling component on the
/// same entity.
#[derive(Component)]
pub struct PickInFlight {
    /// What to dispatch on success.
    pub follow_up: PickFollowUp,
}

/// Fired when a picker resolves with a chosen handle.
///
/// A workbench-side dispatcher (added in a follow-up commit) observes
/// this and translates the [`PickFollowUp`] variant into the matching
/// typed command (`OpenFile { path }`, `SaveAsDocument { doc, path }`,
/// ...).
#[derive(Event, Clone, Debug)]
pub struct PickResolved {
    /// What the original requester wanted done with the result.
    pub follow_up: PickFollowUp,
    /// The chosen handle (always [`StorageHandle::File`] on native).
    pub handle: StorageHandle,
}

/// Fired when the user dismisses a picker without choosing anything.
///
/// Mostly observed for telemetry / status-bar messaging; the default
/// behaviour on cancellation is to do nothing, which is the silent
/// no-op users expect from "X out of a Save dialog".
#[derive(Event, Clone, Debug)]
pub struct PickCancelled {
    /// What would have been dispatched on success.
    pub follow_up: PickFollowUp,
}

// ─────────────────────────────────────────────────────────────────────────────
// Native backend — desktop OS dialogs via `rfd`
// ─────────────────────────────────────────────────────────────────────────────
//
// Lives behind `cfg(not(wasm32))` so the wasm target can ship its own
// dialog-via-File-System-Access observer in the same module without a
// trait or feature wrapper. Same `PickHandle` event in, same
// `PickResolved` / `PickCancelled` events out — call sites don't change.

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use bevy::prelude::*;
    use bevy::tasks::{futures_lite::future, AsyncComputeTaskPool, Task};

    use super::{
        PickCancelled, PickHandle, PickInFlight, PickMode, PickResolved, StorageHandle,
    };

    /// Component holding the in-flight `rfd` dialog future. Spawned on
    /// the same entity as [`PickInFlight`] by [`spawn_picker`]; consumed
    /// by [`drive_picker`].
    #[derive(Component)]
    pub(super) struct NativePickTask(pub Task<Option<StorageHandle>>);

    /// Observer: react to [`PickHandle`] by spawning a background task
    /// that opens the OS dialog. The dialog itself blocks the task's
    /// thread — fine, the task pool tolerates blocking work — but the
    /// UI thread never touches it.
    pub(super) fn spawn_picker(trigger: On<PickHandle>, mut commands: Commands) {
        let event = trigger.event().clone();
        let mode = event.mode;
        let task = AsyncComputeTaskPool::get().spawn(async move { run_dialog_blocking(&mode) });
        commands.spawn((
            PickInFlight {
                follow_up: event.on_resolved,
            },
            NativePickTask(task),
        ));
    }

    /// Per-frame system: poll every in-flight native picker. When one
    /// resolves, fire [`PickResolved`] / [`PickCancelled`] and despawn
    /// the carrier entity. Non-blocking — `poll_once` returns `None`
    /// immediately when the dialog is still up, matching the workspace's
    /// existing task-poll convention (see Modelica's Package Browser).
    pub(super) fn drive_picker(
        mut commands: Commands,
        mut q: Query<(Entity, &mut NativePickTask, &PickInFlight)>,
    ) {
        for (entity, mut task, in_flight) in q.iter_mut() {
            let Some(result) = future::block_on(future::poll_once(&mut task.0)) else {
                continue;
            };
            let follow_up = in_flight.follow_up.clone();
            commands.entity(entity).despawn();
            match result {
                Some(handle) => commands.trigger(PickResolved { follow_up, handle }),
                None => commands.trigger(PickCancelled { follow_up }),
            }
        }
    }

    /// Blocking dialog driver. Runs inside the spawned task, returns
    /// the chosen handle or `None` on cancellation. Always produces a
    /// [`StorageHandle::File`] today — the only backend `rfd` speaks.
    fn run_dialog_blocking(mode: &PickMode) -> Option<StorageHandle> {
        match mode {
            PickMode::OpenFile(filter) => {
                let extensions: Vec<&str> = filter.extensions.iter().map(String::as_str).collect();
                rfd::FileDialog::new()
                    .add_filter(&filter.name, &extensions)
                    .pick_file()
                    .map(StorageHandle::File)
            }
            PickMode::SaveFile(hint) => {
                let mut dialog = rfd::FileDialog::new();
                if let Some(name) = &hint.suggested_name {
                    dialog = dialog.set_file_name(name);
                }
                if let Some(StorageHandle::File(p)) = &hint.start_dir {
                    dialog = dialog.set_directory(p);
                }
                for f in &hint.filters {
                    let extensions: Vec<&str> = f.extensions.iter().map(String::as_str).collect();
                    dialog = dialog.add_filter(&f.name, &extensions);
                }
                dialog.save_file().map(StorageHandle::File)
            }
            PickMode::OpenFolder => rfd::FileDialog::new()
                .pick_folder()
                .map(StorageHandle::File),
        }
    }
}

/// Plugin that wires up the picker backend appropriate for the target.
///
/// On native: registers the `rfd`-driven observer + poll system. On
/// wasm: today it's a no-op stub; the File-System-Access backend will
/// drop into the same plugin behind a `cfg`-gated branch when the wasm
/// build comes online.
///
/// `WorkbenchPlugin` adds this automatically; standalone tests that
/// want picker behaviour without the full workbench shell can install
/// it directly.
pub struct PickerPlugin;

impl Plugin for PickerPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            app.add_observer(native::spawn_picker)
                .add_systems(Update, native::drive_picker);
        }
        #[cfg(target_arch = "wasm32")]
        {
            // Web backend lands in a follow-up: `showOpenFilePicker` /
            // `showSaveFilePicker` / `showDirectoryPicker` via wasm-bindgen,
            // same `PickHandle` in, same `PickResolved` / `PickCancelled`
            // out. Until then, picker requests on wasm silently no-op.
            let _ = app;
        }
    }
}
