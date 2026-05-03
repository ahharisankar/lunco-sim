//! Long-lived [`ModelicaEngine`] exposed as a Bevy resource and
//! kept in lockstep with [`ModelicaDocumentRegistry`].
//!
//! ## Why a long-lived engine
//!
//! The engine wraps a `rumoca_session::Session` whose phase caches
//! (parse, resolve, instantiate, typecheck, flatten, DAE) amortise
//! across every cross-file query the workbench makes — completion,
//! inheritance walks, icon merging, compile, future hover-info.
//! Building a fresh engine per call (the previous shape in
//! `api_queries.rs`) re-uploads every open document on each request;
//! with this handle that work runs once at edit-time and every reader
//! sees the same warm session.
//!
//! ## Concurrency contract
//!
//! - One `Mutex<ModelicaEngine>` per workbench (per-Twin scope today;
//!   becomes per-Twin entry of a map when multi-Twin lands).
//! - Lock calls must be **short**. Snapshot what you need into owned
//!   values and release. A panel that holds the lock across a render
//!   would block API observers, the sync system, and other panels.
//! - Async tasks that need to query the engine can clone the
//!   [`ModelicaEngineHandle`] (it's `Arc`-internal) into the task and
//!   lock there. The MSL static engine in `class_cache::msl_engine` is
//!   independent — process-wide and library-only; this handle covers
//!   the user docs.
//!
//! ## Sync semantics
//!
//! [`drive_engine_sync`] runs every `Update` tick. For each document
//! in the registry it compares the document's generation against the
//! per-doc cursor in [`EngineSyncCursor`]; on a delta it re-upserts
//! the document's current source via
//! [`ModelicaEngine::upsert_document`] (which feeds rumoca's
//! content-hash artifact cache, so unchanged source between two
//! generations is a hashmap hit). Removed documents are flushed via
//! [`ModelicaEngine::close_document`].
//!
//! Lazy-on-edit means a render-frame after the user types lands in
//! the engine on the next system tick — same staleness contract as
//! the per-doc Index.

use bevy::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::engine::ModelicaEngine;
use lunco_doc::{Document, DocumentId};

/// Process-wide handle to the workbench's [`ModelicaEngine`].
///
/// `Clone` is cheap (Arc bump) so callers needing to hand a handle
/// to an async task can do so without holding a Bevy resource borrow.
#[derive(Resource, Clone)]
pub struct ModelicaEngineHandle(Arc<Mutex<ModelicaEngine>>);

impl Default for ModelicaEngineHandle {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(ModelicaEngine::new())))
    }
}

impl ModelicaEngineHandle {
    /// Lock the engine for a query. Panics if the mutex is poisoned
    /// (would mean a previous panic happened while holding the lock —
    /// the engine state is then suspect anyway).
    pub fn lock(&self) -> MutexGuard<'_, ModelicaEngine> {
        self.0.lock().expect("modelica engine mutex poisoned")
    }
}

/// Per-document generation cursor used by [`drive_engine_sync`] to
/// decide which documents need re-upsert this tick. Internal to the
/// sync mechanism.
#[derive(Resource, Default)]
pub struct EngineSyncCursor {
    /// Document → last-seen generation. Absent entry means
    /// "never synced".
    last_synced: HashMap<DocumentId, u64>,
}

/// Sync open Modelica documents into the engine session. Generation-
/// gated: docs whose generation hasn't advanced since the previous
/// sync are no-ops. Docs that have been removed from the registry
/// since last tick are dropped from the engine session via
/// [`ModelicaEngine::close_document`].
///
/// Runs every `Update`. Reads `ModelicaDocumentRegistry`, mutates the
/// engine and the cursor.
pub fn drive_engine_sync(
    handle: Res<ModelicaEngineHandle>,
    registry: Res<crate::ui::state::ModelicaDocumentRegistry>,
    mut cursor: ResMut<EngineSyncCursor>,
) {
    // Collect (doc_id, gen, source) for any document whose generation
    // has advanced. Borrow the registry immutably here; release before
    // we lock the engine to keep the critical section tight.
    let mut to_upsert: Vec<(DocumentId, u64, String)> = Vec::new();
    let mut alive: HashSet<DocumentId> = HashSet::new();
    for (doc_id, host) in registry.iter() {
        alive.insert(doc_id);
        let gen = host.document().generation();
        let needs = match cursor.last_synced.get(&doc_id) {
            Some(prev) => *prev < gen,
            None => true,
        };
        if needs {
            to_upsert.push((doc_id, gen, host.document().source().to_string()));
        }
    }
    let removed: Vec<DocumentId> = cursor
        .last_synced
        .keys()
        .copied()
        .filter(|id| !alive.contains(id))
        .collect();

    if to_upsert.is_empty() && removed.is_empty() {
        return;
    }

    let mut engine = handle.lock();
    for (doc_id, gen, source) in to_upsert {
        match engine.upsert_document(doc_id, &source) {
            Ok(()) => {
                cursor.last_synced.insert(doc_id, gen);
            }
            Err(e) => {
                // A parse failure here doesn't poison anything — the
                // document's strict AST simply isn't queryable from
                // the engine until the user fixes the source. Bump
                // the cursor anyway so we don't retry every tick.
                bevy::log::warn!(
                    "[EngineSync] upsert doc={} gen={} failed: {}",
                    doc_id.raw(),
                    gen,
                    e
                );
                cursor.last_synced.insert(doc_id, gen);
            }
        }
    }
    for doc_id in removed {
        engine.close_document(doc_id);
        cursor.last_synced.remove(&doc_id);
    }
}

/// Plugin registering the engine handle, sync cursor, and sync
/// system. Add once at app build; safe to add multiple times because
/// every component is `init_resource` / unique-system.
pub struct ModelicaEnginePlugin;

impl Plugin for ModelicaEnginePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ModelicaEngineHandle>()
            .init_resource::<EngineSyncCursor>()
            .add_systems(Update, drive_engine_sync);
    }
}
