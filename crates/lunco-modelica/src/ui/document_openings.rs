//! Unified table of in-flight document opens.
//!
//! Replaces three parallel maps that previously each tracked one
//! flavour of "doc N is loading":
//!  - [`crate::ui::panels::package_browser::PackageTreeCache`]'s
//!    `loading_ids` + `file_tasks` (bundled and user-file reads).
//!  - `DrillInLoads` (MSL drill-in slim slices, now folded in here).
//!  - `DuplicateLoads` (duplicate-to-workspace bg parses, now folded
//!    in here).
//!
//! The three flavours have genuinely different task output types
//! and post-install side effects, so each variant of [`OpeningState`]
//! still owns its own typed `Task<...>`. What's unified is the
//! identity (one [`DocumentId`] → one state) and the read surface
//! (`is_loading`, `detail`, `progress`) that overlays and panel
//! gates consult.

use bevy::prelude::*;
use bevy::tasks::Task;
use lunco_doc::DocumentId;
use std::collections::HashMap;
use web_time::Instant;

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
        started: Instant,
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

/// Single-source-of-truth resource for "is doc N still preparing?".
/// Panels read it via [`Self::is_loading`]; drivers iterate its
/// entries filtered to their own variant.
#[derive(Resource, Default)]
pub struct DocumentOpenings {
    pub in_flight: HashMap<DocumentId, OpeningState>,
}

impl DocumentOpenings {
    pub fn is_loading(&self, doc: DocumentId) -> bool {
        self.in_flight.contains_key(&doc)
    }

    /// Short description for the loading overlay — qualified class
    /// name for drill-ins, display name for duplicates and file
    /// reads.
    pub fn detail(&self, doc: DocumentId) -> Option<&str> {
        match self.in_flight.get(&doc)? {
            OpeningState::FileLoad { display_name, .. } => Some(display_name.as_str()),
            OpeningState::DrillIn(b) => Some(b.qualified.as_str()),
            OpeningState::Duplicate(b) => Some(b.display_name.as_str()),
        }
    }

    pub fn drill_in_qualified(&self, doc: DocumentId) -> Option<&str> {
        match self.in_flight.get(&doc)? {
            OpeningState::DrillIn(b) => Some(b.qualified.as_str()),
            _ => None,
        }
    }

    pub fn duplicate_display(&self, doc: DocumentId) -> Option<&str> {
        match self.in_flight.get(&doc)? {
            OpeningState::Duplicate(b) => Some(b.display_name.as_str()),
            _ => None,
        }
    }

    /// `(detail, seconds-since-opened)` for the overlay.
    pub fn progress(&self, doc: DocumentId) -> Option<(&str, f32)> {
        match self.in_flight.get(&doc)? {
            OpeningState::FileLoad { display_name, started, .. } => {
                Some((display_name.as_str(), started.elapsed().as_secs_f32()))
            }
            OpeningState::DrillIn(b) => {
                Some((b.qualified.as_str(), b.started.elapsed().as_secs_f32()))
            }
            OpeningState::Duplicate(b) => {
                Some((b.display_name.as_str(), b.started.elapsed().as_secs_f32()))
            }
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
    // `drainbusy_drops` tick.
    handles.handles.retain(|d, _| still_stale.contains(d));
}

/// Drive [`OpeningState::FileLoad`] entries: poll each pending
/// file-read task, install the resulting document into the registry,
/// and clear the entry. Mirrors the previous `cache.file_tasks`
/// drain that lived in `handle_package_loading_tasks`.
pub fn drive_file_load_openings(
    mut openings: ResMut<DocumentOpenings>,
    mut workbench: ResMut<crate::ui::state::WorkbenchState>,
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
        workbench.diagram_dirty = true;
        workspace.active_document = Some(result.doc_id);
    }
}
