//! Per-document container for in-flight parse tasks.
//!
//! Holds one [`OpeningState`] per [`DocumentId`] until the parse
//! resolves and the driver hands the document to
//! [`crate::ui::state::ModelicaDocumentRegistry`]. Each variant
//! owns its own typed `Task<...>` plus a [`lunco_workbench::status_bus::BusyHandle`]
//! that keeps a `(BusyScope::Document, "opening"|"drill-in"|"duplicate")`
//! entry on the bus for the parse lifetime.
//!
//! **This is not the loading-state authority.** UI panels query the
//! [`lunco_workbench::status_bus::StatusBus`] directly
//! (`bus.is_busy(BusyScope::Document(d.0))` or
//! `bus.lifecycle(...)`) so a single predicate covers every async
//! stage that contributes to a doc's view (parse, projection,
//! reparse, future fetch/index/etc.). The accessors here (`detail`,
//! `progress`, `drill_in_qualified`, `duplicate_display`) return
//! metadata *about* the in-flight task — display name, drill-in
//! target, elapsed time — used by tab-title / placeholder-snapshot
//! code that needs to know what the doc *will be* before it's
//! installed.

use bevy::prelude::*;
use bevy::tasks::Task;
use lunco_doc::DocumentId;
use std::collections::HashMap;

use crate::ui::panels::canvas_diagram::loads::{DrillInBinding, DuplicateBinding};
use crate::ui::panels::package_browser::cache::FileLoadResult;

/// One in-flight document open. Each variant carries the typed
/// `Task<...>` plus the metadata that variant's driver needs to
/// finish the install (drilled-class name, display name, busy
/// handle for the status bus, etc.).
pub enum OpeningState {
    /// Bundled or user-file read driven by the Package Browser. The
    /// task returns a fully-built [`FileLoadResult`]; the driver
    /// installs `result.doc` against `result.doc_id`.
    FileLoad {
        display_name: String,
        task: Task<FileLoadResult>,
        /// RAII guard registered with [`lunco_workbench::status_bus::StatusBus`]
        /// at insert time. Same role as [`DrillInBinding::busy`] and
        /// [`DuplicateBinding::busy`]: keeps a `(Document(doc_id),
        /// "opening")` entry on the bus from "user clicked open" until
        /// the file-load driver hands it off to the projection stage
        /// via [`crate::ui::panels::canvas_diagram::CanvasDiagramState::stash_projection_handoff`].
        busy: lunco_workbench::status_bus::BusyHandle,
    },
    /// MSL drill-in slim-slice load. Built by
    /// [`crate::ui::panels::canvas_diagram::drill_into_class`].
    DrillIn(DrillInBinding),
    /// `Duplicate to Workspace` bg parse. Built by
    /// [`crate::ui::commands::lifecycle::on_duplicate_model_from_read_only`].
    Duplicate(DuplicateBinding),
}

/// Per-document task container. Drivers iterate its entries
/// filtered to their own variant; panels that need *metadata about*
/// an in-flight open (tab title, placeholder snapshot) read via the
/// accessors below. "Is this doc busy?" queries belong on the
/// [`lunco_workbench::status_bus::StatusBus`], not here.
#[derive(Resource, Default)]
pub struct DocumentOpenings {
    pub in_flight: HashMap<DocumentId, OpeningState>,
}

impl DocumentOpenings {
    /// Qualified class name of an in-flight drill-in for `doc`, if
    /// any. Used by placeholder snapshot code (`model_view/context.rs`)
    /// to construct tab titles + URIs before the doc is installed.
    pub fn drill_in_qualified(&self, doc: DocumentId) -> Option<&str> {
        match self.in_flight.get(&doc)? {
            OpeningState::DrillIn(b) => Some(b.qualified.as_str()),
            _ => None,
        }
    }

    /// Display name of an in-flight duplicate for `doc`, if any.
    /// Same placeholder-snapshot role as [`Self::drill_in_qualified`].
    pub fn duplicate_display(&self, doc: DocumentId) -> Option<&str> {
        match self.in_flight.get(&doc)? {
            OpeningState::Duplicate(b) => Some(b.display_name.as_str()),
            _ => None,
        }
    }

    pub fn insert(&mut self, doc: DocumentId, state: OpeningState) {
        self.in_flight.insert(doc, state);
    }

    pub fn remove(&mut self, doc: DocumentId) -> Option<OpeningState> {
        self.in_flight.remove(&doc)
    }

    pub fn get_mut(&mut self, doc: DocumentId) -> Option<&mut OpeningState> {
        self.in_flight.get_mut(&doc)
    }

    pub fn doc_ids(&self) -> Vec<DocumentId> {
        self.in_flight.keys().copied().collect()
    }

    pub fn has_any_drill_in(&self) -> bool {
        self.in_flight
            .values()
            .any(|s| matches!(s, OpeningState::DrillIn(_)))
    }

    pub fn has_any_duplicate(&self) -> bool {
        self.in_flight
            .values()
            .any(|s| matches!(s, OpeningState::Duplicate(_)))
    }
}

/// In-flight per-document `StatusBus` handles for AST reparse —
/// the debounced background parse that runs after a free-form source
/// edit. Distinct from file-load / drill-in / duplicate openings
/// because reparse doesn't have its own typed `Task<...>` we can
/// hang a handle off (parse is dispatched through
/// `ModelicaEngineHandle::upsert_document_async`, which takes a
/// caller-provided spawn callback, plus a wasm worker fallback —
/// too many paths to thread a handle through individually).
///
/// Instead, [`track_ast_reparse_busy`] derives "is reparse in
/// flight?" from the document's own `ast_is_stale()` predicate each
/// frame: rising edge mints a `Document(d) / "reparse"` entry on the
/// bus; falling edge drops it. Renders see continuous busy across
/// typing-debounce → parse → AST-install without a per-edit gap.
#[derive(Resource, Default)]
pub struct AstReparseBusyHandles {
    handles: HashMap<DocumentId, lunco_workbench::status_bus::BusyHandle>,
}

/// Edge-triggered tracker for AST reparse state. Mints a `StatusBus`
/// handle when `ast_is_stale()` flips from false → true and drops it
/// when it flips back. Lets the canvas overlay rely on
/// `bus.lifecycle(Document(d), ...)` alone without an ast-stale
/// fallback predicate.
pub fn track_ast_reparse_busy(
    registry: Res<crate::ui::state::ModelicaDocumentRegistry>,
    mut handles: ResMut<AstReparseBusyHandles>,
    mut bus: ResMut<lunco_workbench::status_bus::StatusBus>,
) {
    use lunco_workbench::status_bus::{BusyScope, StatusBus};
    let mut still_stale: std::collections::HashSet<DocumentId> = Default::default();
    for (doc_id, host) in registry.iter() {
        if !host.document().ast_is_stale() {
            continue;
        }
        still_stale.insert(doc_id);
        if handles.handles.contains_key(&doc_id) {
            continue;
        }
        let h = StatusBus::begin(
            &mut bus,
            BusyScope::Document(doc_id.0),
            "reparse",
            "Reparsing…",
        );
        handles.handles.insert(doc_id, h);
    }
    // Drop handles for docs that are no longer stale (or have been
    // closed). `Drop` clears the bus entry on the next
    // `drain_busy_drops` tick.
    handles.handles.retain(|d, _| still_stale.contains(d));
}

/// Drive [`OpeningState::FileLoad`] entries: poll each pending
/// file-read task, install the resulting document into the registry,
/// and clear the entry. Mirrors the previous `cache.file_tasks`
/// drain that lived in `handle_package_loading_tasks`.
pub fn drive_file_load_openings(
    mut openings: ResMut<DocumentOpenings>,
    mut registry: ResMut<crate::ui::state::ModelicaDocumentRegistry>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
    mut canvas_state: ResMut<crate::ui::panels::canvas_diagram::CanvasDiagramState>,
) {
    use futures_lite::future;
    let doc_ids = openings.doc_ids();
    for doc_id in doc_ids {
        let ready = match openings.get_mut(doc_id) {
            Some(OpeningState::FileLoad { task, .. }) => {
                future::block_on(future::poll_once(task))
            }
            _ => None,
        };
        let Some(result) = ready else { continue };
        // Take the busy handle out of the variant before dropping the
        // rest, and hand it to the canvas state. Bus keeps a
        // `Document(doc_id)` entry continuously across the
        // file-load → projection boundary; the projection spawn
        // releases it via `complete_projection_handoff`.
        if let Some(OpeningState::FileLoad { busy, .. }) = openings.remove(doc_id) {
            canvas_state.stash_projection_handoff(result.doc_id, busy);
        }
        registry.install_prebuilt(result.doc_id, result.doc);
        workspace.active_document = Some(result.doc_id);
    }
}
