//! Shell-level file-workflow commands.
//!
//! Verbs that span every domain — Open, Save All, Save as Twin — live
//! here so all three apps (`lunica`, `rover_sandbox_usd`,
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
//! native picker via [`crate::picker::PickHandle`]; non-empty paths skip the
//! dialog (recents, drag-drop, automation).
//!
//! ## What this module ships
//!
//! - The verbs ([`OpenFile`], [`OpenFolder`], [`OpenTwin`],
//!   [`SaveAll`], [`SaveAsTwin`]) as typed commands.
//! - The picker-resolution router ([`on_pick_resolved`]) that turns
//!   a [`crate::picker::PickResolved`] event into the matching typed verb
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
use bevy::tasks::{AsyncComputeTaskPool, Task};
use lunco_core::{on_command, register_commands, Command};
use lunco_doc_bevy::SaveAsDocument;
use lunco_twin::{DocumentKindId, DocumentKindRegistry, TwinError, TwinMode};

use crate::picker::{PickFollowUp, PickResolved};
use crate::session::{FileRenamed, TwinAdded, TwinClosed, WorkspaceResource};

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
/// [`crate::picker::PickHandle`]) and re-fires this command with the chosen
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

/// Add a folder to the current workspace **without** closing existing
/// folder Twins. VS Code's "Add Folder to Workspace…" semantics.
///
/// Use this when the user wants a multi-root workspace. The companion
/// [`OpenFolder`] command *replaces* the open folder(s) instead.
///
/// Empty `path` triggers a folder picker. Resolved folders are
/// classified the same way as [`OpenFolder`] (presence of `twin.toml`
/// promotes to a Twin spawn).
#[Command(default)]
pub struct AddFolderToWorkspace {
    /// Filesystem path of the folder to add. Empty triggers the picker.
    pub path: String,
}

/// Strict variant of [`AddFolderToWorkspace`] — requires a `twin.toml`
/// in the chosen folder. Used by recents reopen and HTTP callers.
#[Command(default)]
pub struct AddTwin {
    /// Filesystem path of the Twin root (must contain `twin.toml`).
    /// Empty triggers the picker.
    pub path: String,
}

/// Rename a file or folder *inside* an open Twin.
///
/// Identifies the entry by `(twin_root, relative_path)` so the
/// command body is self-contained (no Bevy resource handles) — HTTP
/// callers, scripts, and the inline browser editor all dispatch the
/// same shape. The observer:
///
/// 1. Validates inputs (new_name non-empty, no path separators, source
///    exists, target doesn't already exist).
/// 2. Performs `std::fs::rename` on the absolute paths.
/// 3. Re-scans the affected Twin via [`Twin::reload`] so the file
///    index reflects disk.
/// 4. Patches every open Document whose `DocumentOrigin::File { path }`
///    lay under the old path — paths are rewritten so live edits don't
///    detach from disk.
/// 5. Fires [`FileRenamed`] for domain plugins to chain follow-ups
///    (Modelica class-declaration rename, USD reference rewrites, …).
#[Command(default)]
pub struct RenameTwinEntry {
    /// Absolute path of the Twin root the entry belongs to. The
    /// observer resolves this back to a `TwinId` via
    /// [`WorkspaceResource::twins`].
    pub twin_root: String,
    /// Path of the entry relative to `twin_root` (e.g. `Rover.mo` or
    /// `subdir/Other.mo`).
    pub relative_path: String,
    /// New filename — no path separators allowed (rename only; move
    /// across directories is a separate concern).
    pub new_name: String,
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
fn on_open_folder(
    trigger: On<OpenFolder>,
    mut workspace: ResMut<WorkspaceResource>,
    mut pending: ResMut<PendingTwinOpens>,
    mut commands: Commands,
) {
    use crate::picker::{PickHandle, PickMode};
    let path = trigger.event().path.clone();
    if path.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::OpenFolder,
        });
        return;
    }
    let folder = std::path::Path::new(&path);
    let manifest = folder.join(lunco_twin::MANIFEST_FILENAME);
    if manifest.is_file() {
        info!(
            "[OpenFolder] {} contains {} — routing to OpenTwin",
            path,
            lunco_twin::MANIFEST_FILENAME
        );
        commands.trigger(OpenTwin { path });
        return;
    }
    // VS Code semantics: "Open Folder" *replaces* the current workspace
    // folders. Callers that want to keep existing roots and add another
    // fire `AddFolderToWorkspace` instead.
    close_all_open_folders(&mut workspace, &mut commands, "OpenFolder");
    spawn_twin_from_path(folder, &mut pending, "OpenFolder");
}

#[on_command(OpenTwin)]
fn on_open_twin(
    trigger: On<OpenTwin>,
    mut workspace: ResMut<WorkspaceResource>,
    mut pending: ResMut<PendingTwinOpens>,
    mut commands: Commands,
) {
    use crate::picker::{PickHandle, PickMode};
    let path = trigger.event().path.clone();
    if path.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::OpenTwin,
        });
        return;
    }
    let folder = std::path::Path::new(&path);
    let manifest = folder.join(lunco_twin::MANIFEST_FILENAME);
    if !manifest.is_file() {
        warn!(
            "[OpenTwin] {} has no {} — refusing (use OpenFolder for plain folders)",
            path,
            lunco_twin::MANIFEST_FILENAME
        );
        return;
    }
    close_all_open_folders(&mut workspace, &mut commands, "OpenTwin");
    spawn_twin_from_path(folder, &mut pending, "OpenTwin");
}

#[on_command(AddFolderToWorkspace)]
fn on_add_folder_to_workspace(
    trigger: On<AddFolderToWorkspace>,
    mut pending: ResMut<PendingTwinOpens>,
    mut commands: Commands,
) {
    use crate::picker::{PickHandle, PickMode};
    let path = trigger.event().path.clone();
    if path.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::AddFolderToWorkspace,
        });
        return;
    }
    let folder = std::path::Path::new(&path);
    let manifest = folder.join(lunco_twin::MANIFEST_FILENAME);
    if manifest.is_file() {
        info!(
            "[AddFolderToWorkspace] {} contains {} — routing to AddTwin",
            path,
            lunco_twin::MANIFEST_FILENAME
        );
        commands.trigger(AddTwin { path });
        return;
    }
    spawn_twin_from_path(folder, &mut pending, "AddFolderToWorkspace");
}

#[on_command(AddTwin)]
fn on_add_twin(
    trigger: On<AddTwin>,
    mut pending: ResMut<PendingTwinOpens>,
    mut commands: Commands,
) {
    use crate::picker::{PickHandle, PickMode};
    let path = trigger.event().path.clone();
    if path.is_empty() {
        commands.trigger(PickHandle {
            mode: PickMode::OpenFolder,
            on_resolved: PickFollowUp::AddTwin,
        });
        return;
    }
    let folder = std::path::Path::new(&path);
    let manifest = folder.join(lunco_twin::MANIFEST_FILENAME);
    if !manifest.is_file() {
        warn!(
            "[AddTwin] {} has no {} — refusing (use AddFolderToWorkspace for plain folders)",
            path,
            lunco_twin::MANIFEST_FILENAME
        );
        return;
    }
    spawn_twin_from_path(folder, &mut pending, "AddTwin");
}

/// Close every Twin currently registered in the Workspace, firing
/// [`TwinClosed`] for each. Documents stay open (the data-layer
/// `close_twin` orphans them; re-opening the folder re-associates
/// by path). Used by [`OpenFolder`] / [`OpenTwin`] to implement
/// VS Code's "replace workspace folders" semantics.
fn close_all_open_folders(
    workspace: &mut WorkspaceResource,
    commands: &mut Commands,
    log_tag: &str,
) {
    let ids: Vec<lunco_workspace::TwinId> =
        workspace.twins().map(|(id, _)| id).collect();
    for id in ids {
        workspace.close_twin(id);
        commands.trigger(TwinClosed { twin: id });
        info!("[{log_tag}] closed pre-existing Twin {:?}", id);
    }
}

/// In-flight folder scans. [`TwinMode::open`] walks the filesystem
/// synchronously — large trees (~/.cargo, node_modules, …) easily take
/// seconds to enumerate, and running that on the UI thread freezes
/// the window long enough for the Wayland/X11 compositor to drop the
/// client. Each [`OpenFolder`] / [`OpenTwin`] / [`AddFolderToWorkspace`]
/// / [`AddTwin`] dispatches its scan to [`AsyncComputeTaskPool`] and
/// parks the handle here; [`drain_pending_twin_opens`] polls one frame
/// at a time and registers the Twin once the walker finishes.
#[derive(Resource, Default)]
pub struct PendingTwinOpens {
    tasks: Vec<TwinOpenTask>,
}

struct TwinOpenTask {
    task: Task<Result<TwinMode, TwinError>>,
    path: std::path::PathBuf,
    log_tag: String,
}

/// Shared helper for Open Folder / Open Twin / Add Folder / Add Twin.
///
/// Spawns the scan asynchronously and parks the handle in
/// [`PendingTwinOpens`]. The actual `add_twin` + [`TwinAdded`] firing
/// happens in [`drain_pending_twin_opens`] once the walker returns.
fn spawn_twin_from_path(
    folder: &std::path::Path,
    pending: &mut PendingTwinOpens,
    log_tag: &str,
) {
    let path = folder.to_path_buf();
    let scan_path = path.clone();
    let task = AsyncComputeTaskPool::get()
        .spawn(async move { TwinMode::open(&scan_path) });
    info!("[{log_tag}] scanning {} (off-thread)…", path.display());
    pending.tasks.push(TwinOpenTask {
        task,
        path,
        log_tag: log_tag.to_string(),
    });
}

/// Poll each in-flight folder scan. Ready scans add their Twin to the
/// Workspace and fire [`TwinAdded`]; in-flight ones are kept for the
/// next frame.
pub(crate) fn drain_pending_twin_opens(
    mut pending: ResMut<PendingTwinOpens>,
    mut workspace: ResMut<WorkspaceResource>,
    mut commands: Commands,
) {
    use bevy::tasks::futures_lite::future;
    if pending.tasks.is_empty() {
        return;
    }
    let mut still_running = Vec::with_capacity(pending.tasks.len());
    for mut entry in pending.tasks.drain(..) {
        match future::block_on(future::poll_once(&mut entry.task)) {
            None => still_running.push(entry),
            Some(Ok(TwinMode::Twin(twin))) | Some(Ok(TwinMode::Folder(twin))) => {
                let twin_id = workspace.add_twin(twin);
                commands.trigger(TwinAdded { twin: twin_id });
                info!("[{}] opened {}", entry.log_tag, entry.path.display());
            }
            Some(Ok(TwinMode::Orphan(_))) => {
                warn!(
                    "[{}] {} resolved to Orphan unexpectedly — ignoring",
                    entry.log_tag,
                    entry.path.display()
                );
            }
            Some(Err(e)) => {
                warn!(
                    "[{}] failed to index {}: {e}",
                    entry.log_tag,
                    entry.path.display()
                );
            }
        }
    }
    pending.tasks = still_running;
}

#[on_command(RenameTwinEntry)]
fn on_rename_twin_entry(
    trigger: On<RenameTwinEntry>,
    mut workspace: ResMut<WorkspaceResource>,
    mut commands: Commands,
) {
    use lunco_doc::DocumentOrigin;
    let ev = trigger.event();
    let twin_root = std::path::PathBuf::from(&ev.twin_root);
    let new_name = ev.new_name.trim();
    if new_name.is_empty() {
        warn!("[RenameTwinEntry] new_name is empty");
        return;
    }
    if new_name.contains(std::path::MAIN_SEPARATOR)
        || new_name.contains('/')
        || new_name == "."
        || new_name == ".."
    {
        warn!(
            "[RenameTwinEntry] new_name `{new_name}` contains a path separator or \
             special segment — rename only, no move across directories"
        );
        return;
    }
    // Resolve TwinId by matching root path.
    let twin_id = workspace
        .twins()
        .find(|(_, t)| t.root == twin_root)
        .map(|(id, _)| id);
    let Some(twin_id) = twin_id else {
        warn!(
            "[RenameTwinEntry] no open Twin matches root {}",
            twin_root.display()
        );
        return;
    };
    let old_rel = std::path::PathBuf::from(&ev.relative_path);
    let old_abs = twin_root.join(&old_rel);
    if !old_abs.exists() {
        warn!(
            "[RenameTwinEntry] source missing: {}",
            old_abs.display()
        );
        return;
    }
    let new_abs = old_abs
        .parent()
        .map(|p| p.join(new_name))
        .unwrap_or_else(|| twin_root.join(new_name));
    if new_abs == old_abs {
        // No-op (user submitted the existing name) — silent.
        return;
    }
    if new_abs.exists() {
        warn!(
            "[RenameTwinEntry] target already exists: {}",
            new_abs.display()
        );
        return;
    }
    let is_dir = old_abs.is_dir();
    if let Err(e) = std::fs::rename(&old_abs, &new_abs) {
        warn!(
            "[RenameTwinEntry] fs::rename {} -> {} failed: {e}",
            old_abs.display(),
            new_abs.display()
        );
        return;
    }

    // Re-scan the Twin so its `files()` reflects disk.
    if let Some(twin) = workspace.twin_mut(twin_id) {
        if let Err(e) = twin.reload() {
            warn!(
                "[RenameTwinEntry] Twin::reload after rename failed: {e} \
                 (twin index may be stale until next OpenFolder)"
            );
        }
    }

    // Patch open documents whose canonical path lay under the old path
    // so live edits stay attached to disk.
    for doc in workspace.documents_mut() {
        if let DocumentOrigin::File { path, writable } = &doc.origin {
            if path.starts_with(&old_abs) {
                let suffix = path
                    .strip_prefix(&old_abs)
                    .expect("starts_with implies strip_prefix succeeds");
                let new_path = if suffix.as_os_str().is_empty() {
                    new_abs.clone()
                } else {
                    new_abs.join(suffix)
                };
                let writable = *writable;
                doc.origin = DocumentOrigin::File {
                    path: new_path,
                    writable,
                };
            }
        }
    }

    info!(
        "[RenameTwinEntry] {} -> {}",
        old_abs.display(),
        new_abs.display()
    );
    commands.trigger(FileRenamed {
        twin: twin_id,
        old_abs,
        new_abs,
        is_dir,
    });
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
        PickFollowUp::AddFolderToWorkspace => {
            commands.trigger(AddFolderToWorkspace { path });
        }
        PickFollowUp::AddTwin => {
            commands.trigger(AddTwin { path });
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
    on_add_folder_to_workspace,
    on_add_twin,
    on_new_document,
    on_open_folder,
    on_open_twin,
    on_rename_twin_entry,
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
        // Off-thread folder-scan pipeline: each `Open*` / `Add*` parks
        // a `Task<Result<TwinMode, _>>` in `PendingTwinOpens`; this
        // system polls them every frame and registers Twins as scans
        // complete. Keeps the UI thread responsive on huge trees
        // (`~/.cargo`, `node_modules`, …).
        app.init_resource::<PendingTwinOpens>();
        app.add_systems(Update, drain_pending_twin_opens);
    }
}
