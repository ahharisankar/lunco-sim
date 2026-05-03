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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use crate::engine::ModelicaEngine;
use lunco_doc::{Document, DocumentId};

/// Process-wide accessor for the workbench's engine handle. Set
/// once during plugin init and read from non-Bevy contexts (static
/// helpers in `class_cache`, async tasks, etc.) that can't take a
/// `Res<ModelicaEngineHandle>` parameter.
///
/// Returns `None` before `ModelicaEnginePlugin::build` has run —
/// callers should treat that as "no engine yet" (same as MSL bundle
/// loading: a query before boot returns empty).
static GLOBAL_ENGINE: OnceLock<ModelicaEngineHandle> = OnceLock::new();

pub fn global_engine_handle() -> Option<&'static ModelicaEngineHandle> {
    GLOBAL_ENGINE.get()
}

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
        // Install the resource and mirror it into the process-wide
        // `GLOBAL_ENGINE` slot so static helpers (`class_cache`,
        // off-thread projection tasks) read the same handle the
        // resource exposes. The clone is `Arc`-cheap.
        let handle = ModelicaEngineHandle::default();
        let _ = GLOBAL_ENGINE.set(handle.clone());
        app.insert_resource(handle)
            .init_resource::<EngineSyncCursor>()
            .init_resource::<MslBootstrapState>()
            .add_systems(Update, (drive_engine_sync, drive_msl_bootstrap));
    }
}

/// Tracks whether the MSL bundle has been bootstrapped into the
/// workspace engine. Once `Done`, `drive_msl_bootstrap` becomes a
/// no-op for the rest of the session.
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
enum MslBootstrapState {
    #[default]
    Pending,
    Done,
}

/// Bevy system: when the MSL bundle becomes ready, install it as a
/// `DurableExternal` source root in the workspace engine in one
/// bulk operation. Runs once per session — flips
/// [`MslBootstrapState`] to `Done` and idles thereafter.
///
/// **Web fast path**: when `msl_remote::global_parsed_msl()` returns
/// pre-parsed `Vec<(uri, StoredDefinition)>` from the asset bundle,
/// we route through [`rumoca_session::Session::replace_parsed_source_set`]
/// — zero re-parsing, just register the parsed defs as a source root.
///
/// **Native path**: when the bundle is filesystem-resident
/// (`MslAssetSource::Filesystem`), we leave the source root unbootstrapped
/// here. The lazy fallback in `class_cache::peek_or_load_msl_class`
/// reads individual files into the session via `add_document` on
/// first miss. Tradeoff: native pays per-class parse cost amortised
/// over the session, vs eager full-MSL parse at boot.
fn drive_msl_bootstrap(
    handle: Res<ModelicaEngineHandle>,
    msl_state: Option<Res<lunco_assets::msl::MslLoadState>>,
    mut bootstrap: ResMut<MslBootstrapState>,
) {
    if matches!(*bootstrap, MslBootstrapState::Done) {
        return;
    }
    let Some(state) = msl_state else { return };
    if !matches!(*state, lunco_assets::msl::MslLoadState::Ready { .. }) {
        return;
    }
    // Pre-parsed bundle path (web): bulk-install via the parsed-set
    // API. Both the source bytes and the strict AST live in
    // `GLOBAL_PARSED_MSL`; we just hand the AST half over.
    let parsed = crate::msl_remote::global_parsed_msl();
    if let Some(docs) = parsed {
        let defs: Vec<(String, rumoca_session::parsing::ast::StoredDefinition)> =
            docs.iter().map(|(u, d)| (u.clone(), d.clone())).collect();
        let count = defs.len();
        let mut engine = handle.lock();
        engine.session_mut().replace_parsed_source_set(
            "msl",
            rumoca_session::compile::SourceRootKind::DurableExternal,
            defs,
            None,
        );
        bevy::log::info!(
            "[EngineBootstrap] installed MSL into workspace engine: {} pre-parsed docs",
            count
        );
        *bootstrap = MslBootstrapState::Done;
        return;
    }
    // Native filesystem path: leave eager bulk load deferred — the
    // lazy fallback in `class_cache::peek_or_load_msl_class` covers
    // it. Mark as `Done` so we don't re-check every tick; if a
    // `MslAssetSource::Filesystem` user later wants eager preload,
    // an explicit command can call `engine.load_library_files`
    // against `lunco_assets::msl_dir()`.
    bevy::log::info!(
        "[EngineBootstrap] MSL ready; native fs path stays lazy (workspace engine populated on demand)"
    );
    *bootstrap = MslBootstrapState::Done;
}
