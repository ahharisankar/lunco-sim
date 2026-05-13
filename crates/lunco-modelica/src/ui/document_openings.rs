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
        /// at insert time. Same role as [`DrillInBinding::_busy`] and
        /// [`DuplicateBinding::_busy`]: keeps a `(Document(doc_id),
        /// "opening")` entry on the bus from "user clicked open" until
        /// the file-load driver hands it off to the projection stage
        /// via [`crate::ui::panels::canvas_diagram::CanvasDiagramState::stash_projection_handoff`].
        _busy: lunco_workbench::status_bus::BusyHandle,
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
        if let Some(OpeningState::FileLoad { _busy, .. }) = openings.remove(doc_id) {
            canvas_state.stash_projection_handoff(result.doc_id, _busy);
        }
        registry.install_prebuilt(result.doc_id, result.doc);
        workbench.diagram_dirty = true;
        workspace.active_document = Some(result.doc_id);
    }
}
