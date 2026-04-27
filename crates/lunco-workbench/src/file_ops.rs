//! Shell-level file-workflow commands.
//!
//! Verbs that span every domain — Open, Save All, Save as Twin — live
//! here so all three apps (`modelica_workbench`, `rover_sandbox_usd`,
//! `lunco_client`) get the same File menu, keybinds, and HTTP API
//! shape from one place. Domain-specific commands (`SaveDocument`,
//! `SaveAsDocument`, `CloseDocument`) stay in `lunco-doc-bevy`; their
//! observers continue to live in domain crates because writing a
//! Modelica `.mo` and writing a USD `.usda` differ in details.
//!
//! ## Pattern
//!
//! Every verb is a typed `#[Command]` per `AGENTS.md` § 4.2 — UI
//! clicks, menu items, keybinds, HTTP API calls, MCP tools, and AI
//! agents dispatch the same shape. Empty-string path fields fire the
//! native picker via [`picker::PickHandle`]; non-empty paths skip the
//! dialog (recents, drag-drop, automation).
//!
//! ## What this module ships
//!
//! - The verbs ([`OpenFile`], [`OpenFolder`], [`OpenTwin`],
//!   [`SaveAll`], [`SaveAsTwin`]) as typed commands.
//! - The picker-resolution router ([`on_pick_resolved`]) that turns
//!   a [`picker::PickResolved`] event into the matching typed verb
//!   with the chosen path filled in.
//! - [`FileOpsPlugin`] which registers the above.
//!
//! ## What's deferred
//!
//! - **Observers for [`OpenFolder`] / [`OpenTwin`]** are stubs —
//!   classification (Folder vs Twin via `twin.toml` presence) and Twin
//!   spawning move here in a follow-up.
//! - **[`OpenFile`] observer** lives in `lunco-modelica` today; will
//!   become a generic classifier-and-dispatch when a second domain
//!   contributes.
//! - **`SaveAsDocument` path threading** — `lunco-doc-bevy::SaveAsDocument`
//!   doesn't yet carry a `path`. Until it does, the [`PickFollowUp::SaveAs`]
//!   branch of [`on_pick_resolved`] only logs.
//! - **[`SaveAll`] / [`SaveAsTwin`]** observers are stubs.

use bevy::prelude::*;
use lunco_core::{on_command, register_commands, Command};

use crate::picker::{PickFollowUp, PickResolved};

/// Open a file at `path` into a new tab.
///
/// Empty `path` triggers a native Open-File picker (via
/// [`picker::PickHandle`]) and re-fires this command with the chosen
/// path on success. A non-empty `path` skips the dialog — that's how
/// HTTP automation, recents, and drag-drop reach the same code path.
///
/// The actual loading is domain-specific: `lunco-modelica` observes
/// this and reads `.mo` files into its document registry. When more
/// domains contribute, this evolves into a classifier-and-dispatch
/// (`FileKind::classify` → fire the matching domain command).
#[Command(default)]
pub struct OpenFile {
    /// Filesystem path or URI (`bundled://`, `mem://`). Empty triggers
    /// the picker.
    pub path: String,
}

/// Open a folder (no `twin.toml` requirement).
///
/// Empty `path` triggers a native folder picker. Resolved folders are
/// classified at the observer level: presence of `twin.toml` promotes
/// to a Twin (equivalent to firing [`OpenTwin`]); absence opens it as
/// a plain folder workspace.
#[Command(default)]
pub struct OpenFolder {
    /// Filesystem path of the folder to open. Empty triggers the picker.
    pub path: String,
}

/// Open a Twin folder — strict variant of [`OpenFolder`] that errors
/// if the chosen folder lacks a `twin.toml`.
///
/// Used by recent-Twins reopens, the Welcome screen's "Open Twin"
/// button, and HTTP callers that explicitly want Twin semantics.
/// Generic "Open Folder" callers should use [`OpenFolder`] and let
/// the observer classify.
#[Command(default)]
pub struct OpenTwin {
    /// Filesystem path of the Twin root (must contain `twin.toml`).
    /// Empty triggers the picker.
    pub path: String,
}

/// Save every dirty document in the current session.
///
/// Documents with a writable canonical path are written via their
/// owning domain's [`SaveDocument`](lunco_doc_bevy::SaveDocument)
/// observer. Drafts (Untitled documents) need user input for their
/// destination — when a Twin is open they can be batch-promoted via
/// the Save-All-into-Twin dialog (see `13-twin-and-workflow.md` § 7a);
/// otherwise the user is offered a Save-as-Twin promotion.
#[Command(default)]
pub struct SaveAll {}

/// Promote the current session into a Twin at `folder`.
///
/// Writes a minimal `twin.toml` to the chosen folder, registers all
/// open documents under it, and rewrites cross-references from draft
/// `mem://` URIs to their new on-disk paths. Empty `folder` triggers
/// a folder picker.
#[Command(default)]
pub struct SaveAsTwin {
    /// Target folder for the new Twin's `twin.toml`. Empty triggers
    /// the picker.
    pub folder: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Stub observers — flesh out in follow-up commits
// ─────────────────────────────────────────────────────────────────────────────

#[on_command(OpenFolder)]
fn on_open_folder(trigger: On<OpenFolder>, mut commands: Commands) {
    use crate::picker::{PickHandle, PickMode};
    let path = trigger.event().path.clone();
    if path.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::OpenFolder,
        });
        return;
    }
    // Classification (`twin.toml` presence → Twin vs plain Folder)
    // and Twin spawning land in a follow-up. Today: log the request.
    info!("[OpenFolder] path={} (handler stubbed)", path);
}

#[on_command(OpenTwin)]
fn on_open_twin(trigger: On<OpenTwin>, mut commands: Commands) {
    use crate::picker::{PickHandle, PickMode};
    let path = trigger.event().path.clone();
    if path.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::OpenTwin,
        });
        return;
    }
    info!("[OpenTwin] path={} (handler stubbed)", path);
}

#[on_command(SaveAll)]
fn on_save_all(_trigger: On<SaveAll>) {
    info!("[SaveAll] handler stubbed — iterating dirty docs lands in follow-up");
}

#[on_command(SaveAsTwin)]
fn on_save_as_twin(trigger: On<SaveAsTwin>, mut commands: Commands) {
    use crate::picker::{PickHandle, PickMode};
    let folder = trigger.event().folder.clone();
    if folder.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::SaveAsTwin,
        });
        return;
    }
    info!("[SaveAsTwin] folder={} (handler stubbed)", folder);
}

// ─────────────────────────────────────────────────────────────────────────────
// Picker resolution → typed command
// ─────────────────────────────────────────────────────────────────────────────

/// Translate a [`PickResolved`] event into the matching typed
/// file-workflow command, with the chosen path filled in.
///
/// Cancellations ([`picker::PickCancelled`]) are silent by design —
/// no observer here for them. Add one if you want telemetry.
fn on_pick_resolved(trigger: On<PickResolved>, mut commands: Commands) {
    let ev = trigger.event();
    let Some(path) = ev.handle.as_file_path().map(|p| p.display().to_string()) else {
        warn!(
            "[PickResolved] non-file handle — picker backend produced something \
             other than `StorageHandle::File`; ignoring"
        );
        return;
    };
    match &ev.follow_up {
        PickFollowUp::OpenFile => {
            commands.trigger(OpenFile { path });
        }
        PickFollowUp::OpenFolder => {
            commands.trigger(OpenFolder { path });
        }
        PickFollowUp::OpenTwin => {
            commands.trigger(OpenTwin { path });
        }
        PickFollowUp::SaveAs(_doc) => {
            // `lunco-doc-bevy::SaveAsDocument` does not yet carry a
            // `path` field; threading the picked path through happens
            // in a follow-up that extends the struct + updates each
            // domain's observer. Until then, log and drop.
            warn!(
                "[PickResolved] SaveAs path={} dropped — \
                 SaveAsDocument.path threading is not yet wired",
                path
            );
        }
        PickFollowUp::SaveAsTwin => {
            commands.trigger(SaveAsTwin { folder: path });
        }
    }
}

// `register_commands!()` registers each command's type + observer in
// one call. `on_pick_resolved` is *not* in this list — it observes a
// non-Command event (`PickResolved`) and is added directly in the
// plugin's `build`. `OpenFile` is also absent: the observer that
// loads `.mo` content lives in `lunco-modelica` and registers itself
// there; the workbench owns only the typed struct.
register_commands!(on_open_folder, on_open_twin, on_save_all, on_save_as_twin);

/// Plugin that registers shell-level file-workflow commands.
///
/// Auto-installed by `WorkbenchPlugin`. Headless tests that want
/// these commands without the full dock shell can add it directly.
pub struct FileOpsPlugin;

impl Plugin for FileOpsPlugin {
    fn build(&self, app: &mut App) {
        register_all_commands(app);
        // OpenFile is owned by this crate but its observer lives in
        // domain crates (modelica today). Register the type here so
        // HTTP-API introspection sees it even before any domain
        // crate registers an observer. Idempotent — re-registration
        // by a domain's `register_commands!()` is a no-op.
        app.register_type::<OpenFile>();
        app.add_observer(on_pick_resolved);
    }
}
