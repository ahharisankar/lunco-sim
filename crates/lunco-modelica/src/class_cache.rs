//! Engine-backed MSL class loader.
//!
//! Routes all MSL class lookups through the workbench's single
//! [`crate::engine_resource::ModelicaEngineHandle`] (workspace +
//! libraries unified). Misses on `peek_or_load_msl_class_blocking` resolve a
//! qualified name to a file via [`crate::library_fs`], read source
//! bytes from `lunco_assets::msl::global_msl_source`, and feed the
//! result into the workspace engine's session via `add_document`.
//!
//! ## Why one engine
//!
//! Earlier the MSL class cache lived in a separate process-wide
//! `Session` (a `Mutex<ModelicaEngine>` here in `class_cache.rs`)
//! disjoint from the workspace engine that holds user docs. That
//! split made `class_inherited_annotations_query` for a user class
//! that extends an MSL base return empty — the workspace engine
//! couldn't see the base. Routing both into one session resolves
//! cross-tier inheritance walks naturally.
//!
//! ## Bootstrap timing
//!
//! Web: `engine_resource::drive_msl_bootstrap` calls
//! `replace_parsed_source_set("msl", DurableExternal, …)` once when
//! `MslLoadState::Ready` flips and `GLOBAL_PARSED_MSL` is populated.
//! After that point every MSL class is resolvable without per-class
//! disk I/O.
//!
//! Native: bootstrap stays lazy — the system above logs and idles,
//! and the helpers below pull individual `.mo` files into the
//! session via `add_document` on first miss. Same content-hash
//! cache backs both paths.

use std::sync::Arc;

use crate::library_fs::{locate_library_file, resolve_class_path_indexed};

/// MSL class-lookup behaviour for resolver helpers in `diagram` and
/// `canvas_projection`. Replaces the `&dyn Fn(&str) -> Option<...>`
/// parameter that used to thread one of two static fn pointers
/// through every helper.
///
/// Both modes route through the workspace [`crate::engine::ModelicaEngine`]
/// (engine consolidation) — they differ only in what to do on a miss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MslLookupMode {
    /// Cache-only: a miss returns `None`. Use from off-thread tasks
    /// that must not block on rumoca parses (notably the canvas
    /// projection task on `AsyncComputeTaskPool`). The icon
    /// resolver falls back to defaults until a later edit / drill-in
    /// warms the engine session.
    Cached,
    /// Load on miss: the call blocks the thread to read + parse the
    /// missing file into the engine session. Safe from the main
    /// thread / tests / observers; risky from off-thread tasks
    /// where the lock contention can stall the deadline.
    Loading,
}

impl MslLookupMode {
    /// Resolve `qualified` using this mode's policy.
    pub fn lookup(
        self,
        qualified: &str,
    ) -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
        match self {
            Self::Cached => peek_msl_class_cached(qualified),
            Self::Loading => peek_or_load_msl_class_blocking(qualified),
        }
    }
}

/// Read MSL source bytes for a relative path, going through the
/// process-wide [`lunco_assets::msl::MslAssetSource`]. Returns
/// `None` if the source hasn't been installed yet (web boot before
/// fetch completes) or the path isn't present.
fn read_msl_source_bytes(path: &std::path::Path) -> Option<String> {
    let source = lunco_assets::msl::global_msl_source()?;
    let bytes = source.read(path)?;
    String::from_utf8(bytes).ok()
}

/// Resolve a fully-qualified MSL class name to its `Arc<ClassDef>`
/// against the workbench's workspace engine. Loads the containing
/// file into the session on first miss; cheap (HashMap hit) on
/// every subsequent call once warm.
///
/// Returns `None` if the engine handle isn't installed yet (early
/// boot) or the file can't be located. Behaviour at the call sites
/// matches the previous static-MSL-engine implementation: a None
/// during boot lets icon/connector resolvers fall back to defaults
/// until MSL lands.
pub fn peek_or_load_msl_class_blocking(
    qualified: &str,
) -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
    let handle = crate::engine_resource::global_engine_handle()?;
    let mut engine = handle.lock();
    if !engine.has_class(qualified) {
        let path = resolve_class_path_indexed(qualified)
            .or_else(|| locate_library_file(qualified))?;
        let source = read_msl_source_bytes(&path)?;
        let uri = path.to_string_lossy().replace('\\', "/");
        engine.session_mut().add_document(&uri, &source).ok()?;
    }
    engine.class_def(qualified).map(Arc::new)
}

/// Non-blocking variant of [`peek_or_load_msl_class_blocking`] — returns the
/// `Arc<ClassDef>` if the engine session already holds it, and
/// `None` *without triggering a load* on a miss.
///
/// Use this from hot paths that must not block on rumoca parse —
/// notably the projection task running on Bevy's AsyncComputeTaskPool,
/// where a sync MSL parse from inside a worker that's already serving
/// a parent rumoca parse stalls for the projection deadline.
pub fn peek_msl_class_cached(
    qualified: &str,
) -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
    let handle = crate::engine_resource::global_engine_handle()?;
    let mut engine = handle.lock();
    if !engine.has_class(qualified) {
        return None;
    }
    engine.class_def(qualified).map(Arc::new)
}
