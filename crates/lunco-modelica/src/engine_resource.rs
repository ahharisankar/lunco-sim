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

    /// Spawn an off-thread strict parse for `doc_id`'s `source` and
    /// install the resulting AST into the session when it completes.
    ///
    /// Returns immediately; the lock is held only briefly to mark
    /// `doc_id` as pending. The parse itself runs OUTSIDE the lock,
    /// then a brief lock at the end installs the AST and queues a
    /// completion via [`ModelicaEngine::finish_parse`].
    ///
    /// `gen` is the doc's generation at spawn time — readers that
    /// drain completions can compare it against the doc's current
    /// generation and discard stale results.
    ///
    /// `spawn_fn` is the platform task spawner: native callers pass
    /// `|task| AsyncComputeTaskPool::get().spawn(async move { task() }).detach()`.
    /// WASM can pass an equivalent. Decoupling the spawner keeps this
    /// crate Bevy-agnostic at the engine layer.
    ///
    /// No-op if a parse for `doc_id` is already in flight (dedupe).
    pub fn upsert_document_async<F>(
        &self,
        doc_id: DocumentId,
        gen: u64,
        source: String,
        spawn_fn: F,
    ) where
        F: FnOnce(Box<dyn FnOnce() + Send + 'static>),
    {
        // Reserve the in-flight slot. Bail if another parse is running
        // for this doc — the next sync tick will pick up newer source
        // when the current parse finishes.
        let uri = {
            let mut engine = self.lock();
            if !engine.mark_pending(doc_id) {
                return;
            }
            engine.uri_for(doc_id)
        };
        let me = ModelicaEngineHandle(Arc::clone(&self.0));
        spawn_fn(Box::new(move || {
            let parsed = rumoca_phase_parse::parse_to_ast(&source, &uri).ok();
            let mut engine = me.lock();
            match parsed {
                Some(ast) => engine.install_parsed_ast(doc_id, ast),
                None => engine.install_lenient(doc_id, &source),
            }
            engine.finish_parse(doc_id, gen);
        }));
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
    // Collect (doc_id, gen, ast?, source) for any document whose
    // generation has advanced. If the doc already parsed strictly we
    // hand the engine the AST directly via `add_parsed_batch` — saves
    // ~370ms per upsert vs the engine reparsing the same source. Fall
    // back to source-only for docs whose strict parse failed (engine
    // will run the lenient parser as before).
    let mut to_upsert: Vec<(
        DocumentId,
        u64,
        Option<std::sync::Arc<rumoca_session::parsing::ast::StoredDefinition>>,
        String,
    )> = Vec::new();
    let mut alive: HashSet<DocumentId> = HashSet::new();
    for (doc_id, host) in registry.iter() {
        alive.insert(doc_id);
        let gen = host.document().generation();
        let needs = match cursor.last_synced.get(&doc_id) {
            Some(prev) => *prev < gen,
            None => true,
        };
        if needs {
            let ast = host.document().strict_ast();
            to_upsert.push((doc_id, gen, ast, host.document().source().to_string()));
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

    // Drain any async parse completions from previous ticks. Each
    // completion means the engine session is now up-to-date for that
    // doc — bump the cursor so we don't re-spawn. Bevy adapters can
    // observe the completion and trigger Index rebuilds in a follow-up
    // (today the doc's `apply_patch` already calls `rebuild_index`).
    {
        let completed = handle.lock().drain_completed();
        for (doc_id, gen) in completed {
            bevy::log::info!(
                "[EngineSync] async parse complete doc={} gen={}",
                doc_id.raw(),
                gen,
            );
            // Only advance cursor if we don't already have a newer
            // sync. (User edits could have overshot the gen the parse
            // started against — a fresh parse is queued already if so.)
            let current = cursor.last_synced.get(&doc_id).copied().unwrap_or(0);
            if gen > current {
                cursor.last_synced.insert(doc_id, gen);
            }
        }
    }

    let mut engine = handle.lock();
    for (doc_id, gen, ast, source) in to_upsert {
        let src_len = source.len();
        let t = std::time::Instant::now();
        if let Some(ast) = ast {
            // Pre-parsed fast path: the doc already has a strict AST.
            // Hand it to rumoca's session via `add_parsed_batch` so
            // the engine doesn't redo the same parse. Cloning the
            // `StoredDefinition` is unavoidable today (the API takes
            // by-value); the per-class `Arc`s inside it keep this
            // shallow.
            engine.upsert_document_with_ast(doc_id, (*ast).clone());
            bevy::log::info!(
                "[EngineSync] upsert(parsed) doc={} gen={} src={}B in {:.1}ms",
                doc_id.raw(),
                gen,
                src_len,
                t.elapsed().as_secs_f64() * 1000.0,
            );
            cursor.last_synced.insert(doc_id, gen);
            continue;
        }
        // No pre-parsed AST → async path. Don't block the Update tick
        // on a parse; spawn off-thread, drain the completion next
        // tick. The engine itself dedupes: re-firing for the same
        // doc while a parse is in flight is a no-op.
        if engine.is_doc_pending(doc_id) {
            // Already parsing; will resolve on a future tick. Don't
            // bump the cursor — let the completion drain do that.
            continue;
        }
        // Drop the engine guard before spawning so the worker can
        // re-acquire when it finishes.
        drop(engine);
        let pool = bevy::tasks::AsyncComputeTaskPool::get();
        handle.upsert_document_async(doc_id, gen, source, |task| {
            pool.spawn(async move { task() }).detach();
        });
        bevy::log::info!(
            "[EngineSync] async parse spawned doc={} gen={} src={}B",
            doc_id.raw(),
            gen,
            src_len,
        );
        // Re-acquire for the rest of the loop. (Hot path is the
        // pre-parsed branch above — async path runs only when the
        // doc lacks a strict AST, e.g. lazy-created docs after
        // Phase 2 lands.)
        engine = handle.lock();
        // No cursor bump here — the drain step above advances it
        // when this parse completes.
        let _ = src_len;
        let _ = t;
        continue;
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
