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
        let bytes = source.len();
        spawn_fn(Box::new(move || {
            let t_total = std::time::Instant::now();
            // Lenient parser: always produces a usable tree.
            let t_parse = std::time::Instant::now();
            let recovery = rumoca_phase_parse::parse_to_syntax(&source, &uri);
            let parse_ms = t_parse.elapsed().as_secs_f64() * 1000.0;
            let has_errors = recovery.has_errors();
            let ast = recovery.best_effort().clone();
            let t_install = std::time::Instant::now();
            let mut engine = me.lock();
            engine.install_parsed_ast(doc_id, ast);
            engine.finish_parse(doc_id, gen);
            let install_ms = t_install.elapsed().as_secs_f64() * 1000.0;
            bevy::log::info!(
                "[engine] async parse doc={} gen={} bytes={} parse={:.1}ms install={:.1}ms total={:.1}ms has_errors={}",
                doc_id.raw(),
                gen,
                bytes,
                parse_ms,
                install_ms,
                t_total.elapsed().as_secs_f64() * 1000.0,
                has_errors,
            );
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
/// Edit-debounce window before re-parsing a document that was
/// previously parsed. New docs (never parsed) spawn immediately —
/// only the edit path is debounced. Mirrors the prior `ast_refresh`
/// gate now that `drive_engine_sync` is the single parse driver.
pub const AST_DEBOUNCE_MS: u128 = 2500;

pub fn drive_engine_sync(
    handle: Res<ModelicaEngineHandle>,
    mut registry: ResMut<crate::ui::state::ModelicaDocumentRegistry>,
    mut cursor: ResMut<EngineSyncCursor>,
    activity: Res<crate::ui::input_activity::InputActivity>,
) {
    // ── 1. Drain async-parse completions ──────────────────────────────
    // Pull every completion the workers have queued since the last
    // tick. For each, fetch the strict AST from the session and
    // backfill the doc's local SyntaxCache + AstCache so panels see
    // the parsed state without needing a separate `ast_refresh` pass.
    let completed = handle.lock().drain_completed();
    for (doc_id, parse_gen) in completed {
        // Snapshot current doc gen + URI under a brief engine lock —
        // we'll backfill if the doc still matches the gen this parse
        // ran against.
        let host_gen = registry
            .host(doc_id)
            .map(|h| h.document().generation())
            .unwrap_or(u64::MAX);
        if parse_gen != host_gen {
            // Doc moved on while parse was in flight; the next
            // sync tick will spawn a fresh parse for the new gen.
            bevy::log::info!(
                "[EngineSync] async parse stale (parse_gen={parse_gen} doc_gen={host_gen}) — discarded for doc={}",
                doc_id.raw(),
            );
            continue;
        }
        let parsed_ast = handle.lock().parsed_for_doc(doc_id).cloned();
        match (parsed_ast, registry.host_mut(doc_id)) {
            (Some(ast), Some(host)) => {
                let arc_ast = std::sync::Arc::new(ast);
                let syntax = crate::document::SyntaxCache {
                    generation: parse_gen,
                    ast: arc_ast,
                    has_errors: false,
                };
                let ast_cache = crate::document::AstCache {
                    generation: parse_gen,
                    result: Ok(()),
                };
                host.document_mut().install_parse_results(ast_cache, syntax);
                bevy::log::info!(
                    "[EngineSync] async parse complete doc={} gen={} → backfilled doc.syntax",
                    doc_id.raw(),
                    parse_gen,
                );
            }
            (None, _) => {
                // Strict parse failed (recovered into session via
                // lenient fallback). Mark doc's AstCache Err so
                // diagnostics panel knows.
                if let Some(host) = registry.host_mut(doc_id) {
                    let syntax = crate::document::SyntaxCache {
                        generation: parse_gen,
                        ast: std::sync::Arc::new(
                            rumoca_session::parsing::ast::StoredDefinition::default(),
                        ),
                        has_errors: true,
                    };
                    let ast_cache = crate::document::AstCache {
                        generation: parse_gen,
                        result: Err("strict parse failed (lenient recovered)".into()),
                    };
                    host.document_mut().install_parse_results(ast_cache, syntax);
                }
                bevy::log::warn!(
                    "[EngineSync] async parse strict-failed doc={} gen={}",
                    doc_id.raw(),
                    parse_gen,
                );
            }
            (Some(_), None) => {
                // Doc was closed mid-parse; engine still got the AST.
            }
        }
        let current = cursor.last_synced.get(&doc_id).copied().unwrap_or(0);
        if parse_gen > current {
            cursor.last_synced.insert(doc_id, parse_gen);
        }
    }

    // ── 2. Collect docs needing sync ──────────────────────────────────
    // For each doc whose generation has advanced past the cursor,
    // decide between sync fast-path (fresh strict AST already on doc)
    // and async path (no AST or stale).
    enum SyncPlan {
        Sync(std::sync::Arc<rumoca_session::parsing::ast::StoredDefinition>),
        Async(String),
    }
    let mut to_upsert: Vec<(DocumentId, u64, SyncPlan)> = Vec::new();
    let mut alive: HashSet<DocumentId> = HashSet::new();
    for (doc_id, host) in registry.iter() {
        alive.insert(doc_id);
        let doc = host.document();
        let gen = doc.generation();
        let needs = match cursor.last_synced.get(&doc_id) {
            Some(prev) => *prev < gen,
            None => true,
        };
        if !needs {
            continue;
        }
        // Fresh strict AST = doc.syntax.generation matches doc.generation
        // and AstCache reports Ok. Otherwise the cached tree is stale
        // (post-edit pre-reparse) and re-using it would push stale
        // bytes into the engine session. Async path handles staleness
        // by re-parsing.
        let fresh_ast = if !doc.syntax_is_stale() && !doc.ast_is_stale() {
            doc.strict_ast()
        } else {
            None
        };
        let plan = match fresh_ast {
            Some(ast) => SyncPlan::Sync(ast),
            None => SyncPlan::Async(doc.source().to_string()),
        };
        to_upsert.push((doc_id, gen, plan));
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

    // ── 3. Apply sync upserts + spawn async parses ────────────────────
    let mut sync_only: Vec<(DocumentId, u64, std::sync::Arc<rumoca_session::parsing::ast::StoredDefinition>)> = Vec::new();
    let mut async_only: Vec<(DocumentId, u64, String)> = Vec::new();
    for (doc_id, gen, plan) in to_upsert {
        match plan {
            SyncPlan::Sync(ast) => sync_only.push((doc_id, gen, ast)),
            SyncPlan::Async(src) => async_only.push((doc_id, gen, src)),
        }
    }
    {
        let mut engine = handle.lock();
        for (doc_id, gen, ast) in sync_only {
            engine.upsert_document_with_ast(doc_id, (*ast).clone());
            bevy::log::info!(
                "[EngineSync] upsert(parsed) doc={} gen={}",
                doc_id.raw(),
                gen,
            );
            cursor.last_synced.insert(doc_id, gen);
        }
        for doc_id in &removed {
            engine.close_document(*doc_id);
        }
    }
    for doc_id in removed {
        cursor.last_synced.remove(&doc_id);
    }
    // Spawn async parses outside the engine lock — the spawn helper
    // re-locks briefly to mark pending; the worker re-locks at the
    // end to install the AST.
    //
    // Debounce gate (replaces the prior `ast_refresh` system):
    //   - First parse for a doc (syntax.generation == 0) fires
    //     immediately — open-flow, user is waiting.
    //   - Edit reparse (syntax.generation > 0 but stale) waits for
    //     `AST_DEBOUNCE_MS` of post-edit silence + no input activity.
    //     Lets a typing burst settle before paying for a parse.
    let pool = bevy::tasks::AsyncComputeTaskPool::get();
    let now = web_time::Instant::now();
    for (doc_id, gen, source) in async_only {
        if handle.lock().is_doc_pending(doc_id) {
            continue;
        }
        // Look up the doc to decide first-parse-vs-edit-reparse.
        let (was_parsed, last_edit) = match registry.host(doc_id) {
            Some(host) => {
                let doc = host.document();
                (
                    doc.syntax_arc().generation > 0,
                    doc.last_source_edit_at(),
                )
            }
            None => (false, None),
        };
        if was_parsed {
            // Edit case: defer until burst settles + UI idles.
            let elapsed_ok = match last_edit {
                Some(t) => now.duration_since(t).as_millis() >= AST_DEBOUNCE_MS,
                None => true,
            };
            if !elapsed_ok || activity.is_active() {
                continue;
            }
        }
        let src_len = source.len();
        handle.upsert_document_async(doc_id, gen, source, |task| {
            pool.spawn(async move { task() }).detach();
        });
        bevy::log::info!(
            "[EngineSync] async parse spawned doc={} gen={} src={}B (first_parse={})",
            doc_id.raw(),
            gen,
            src_len,
            !was_parsed,
        );
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
