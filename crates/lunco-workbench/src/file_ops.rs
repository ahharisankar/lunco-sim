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
//! - **[`SaveAll`] / [`SaveAsTwin`]** observers are stubs.

use bevy::prelude::*;
use lunco_core::{on_command, register_commands, Command};
use lunco_doc_bevy::SaveAsDocument;
use lunco_twin::{DocumentKindId, DocumentKindRegistry};

use crate::picker::{PickFollowUp, PickResolved};

/// Create a new untitled document of the given kind.
///
/// `kind` is the registered [`DocumentKindId`] string (`"modelica"`,
/// `"julia"`, `"usd"`, …). An **empty** `kind` is the "use the
/// default" signal — the workbench-side observer looks up the
/// registry, picks the first kind whose
/// [`can_create_new`](DocumentKindMeta::can_create_new) is true, and
/// re-fires this command with the resolved kind. That's how Ctrl+N
/// reaches a sensible default without the keybind owner having to
/// know which domain crates are loaded.
///
/// Domain crates add observers that gate on `cmd.kind == "<their_id>"`
/// and create the actual document. The workbench's default observer
/// only handles the empty-kind resolution.
#[Command(default)]
pub struct NewDocument {
    /// Registered document kind id, or empty for "default".
    pub kind: String,
}

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

#[on_command(NewDocument)]
fn on_new_document(
    trigger: On<NewDocument>,
    registry: Res<DocumentKindRegistry>,
    mut commands: Commands,
) {
    // Domain-specific creation is handled by domain crates' own
    // observers, gated on `cmd.kind == "<their_id>"`. This observer
    // exists only to resolve the "default" sentinel (empty `kind`)
    // into a real registered id and re-fire — which is what Ctrl+N
    // dispatches when no specific kind was chosen.
    let kind = trigger.event().kind.clone();
    if !kind.is_empty() {
        return;
    }
    // Pick the first registered kind that opts into File→New. UI may
    // surface a "last used" preference later; for now first-found is
    // fine — only Modelica registers today.
    let default_kind: Option<DocumentKindId> = registry
        .iter()
        .find(|(_, m)| m.can_create_new)
        .map(|(id, _)| id.clone());
    let Some(id) = default_kind else {
        warn!("[NewDocument] no document kinds registered with can_create_new=true");
        return;
    };
    commands.trigger(NewDocument {
        kind: id.as_str().to_string(),
    });
}

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
    // Auto-classify: a folder containing `twin.toml` is a Twin —
    // re-trigger as `OpenTwin` so its observer handles the manifest
    // load and spawns the Twin entity. A bare folder (no manifest)
    // gets the plain Folder workspace; today that's a stub log
    // because the Folder mode itself isn't wired up yet.
    let manifest = std::path::Path::new(&path).join(lunco_twin::MANIFEST_FILENAME);
    if manifest.is_file() {
        info!(
            "[OpenFolder] {} contains {} — promoting to Twin",
            path,
            lunco_twin::MANIFEST_FILENAME
        );
        commands.trigger(OpenTwin { path });
        return;
    }
    info!("[OpenFolder] path={} (plain folder mode — handler stubbed)", path);
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
    // Strict-mode validation — the lenient counterpart `OpenFolder`
    // auto-classifies; callers who routed here directly (recents,
    // HTTP, scripts) expect a real Twin and an error otherwise.
    let manifest = std::path::Path::new(&path).join(lunco_twin::MANIFEST_FILENAME);
    if !manifest.is_file() {
        warn!(
            "[OpenTwin] {} has no {} — refusing (use OpenFolder for plain folders)",
            path,
            lunco_twin::MANIFEST_FILENAME
        );
        return;
    }
    info!("[OpenTwin] path={} (Twin spawn — handler stubbed)", path);
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
        PickFollowUp::SaveAs(doc) => {
            commands.trigger(SaveAsDocument { doc: *doc, path });
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
register_commands!(
    on_new_document,
    on_open_folder,
    on_open_twin,
    on_save_all,
    on_save_as_twin
);

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
