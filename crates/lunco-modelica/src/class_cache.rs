//! Engine-backed MSL class loader.
//!
//! Owns the process-wide [`ModelicaEngine`] used as the MSL class
//! cache. Misses on `peek_or_load_msl_class` resolve a qualified
//! name to a file (via [`crate::library_fs`]), read source bytes
//! from `lunco_assets::msl::global_msl_source` (filesystem native /
//! in-memory bundle web), and feed it into the engine's rumoca
//! session via `add_document`. Subsequent lookups hit rumoca's
//! per-file fingerprint cache + in-memory LRU.
//!
//! Drill-in / open-tab flows DON'T go through here — they spawn an
//! `AsyncComputeTaskPool` task that calls
//! [`crate::document::ModelicaDocument::load_msl_file`] directly.
//! That returns a fully-built document; the polling driver
//! ([`crate::ui::panels::canvas_diagram::drive_drill_in_loads`])
//! installs it on the main thread. Same source-of-truth (rumoca's
//! content-hash artifact cache) without a separate two-tier
//! `FileCache` / `ClassCache` infrastructure.

use std::sync::Arc;

use crate::library_fs::{locate_library_file, resolve_class_path_indexed};

/// Process-wide [`ModelicaEngine`] holding every MSL class touched
/// in this session. The engine's rumoca session IS the cache —
/// there is no parallel HashMap. First miss for a qualified name
/// reads the containing `.mo`, parses it into the session, and
/// caches the result through rumoca's content-hash machinery.
fn msl_engine() -> &'static std::sync::Mutex<crate::engine::ModelicaEngine> {
    use std::sync::{Mutex, OnceLock};
    static ENGINE: OnceLock<Mutex<crate::engine::ModelicaEngine>> = OnceLock::new();
    ENGINE.get_or_init(|| Mutex::new(crate::engine::ModelicaEngine::new()))
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

/// Resolve a fully-qualified MSL class name to its `Arc<ClassDef>`.
/// Loads the containing file into the engine session on first miss.
/// Cheap (HashMap hit) on every subsequent call.
///
/// Synchronous + blocking — locks the global engine mutex during
/// the parse. Safe to call from `AsyncComputeTaskPool::spawn` task
/// bodies; on the main thread, prefer the `_cached` variant in
/// hot paths.
pub fn peek_or_load_msl_class(
    qualified: &str,
) -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
    let mut engine = msl_engine().lock().ok()?;
    if !engine.has_class(qualified) {
        let path = resolve_class_path_indexed(qualified)
            .or_else(|| locate_library_file(qualified))?;
        let source = read_msl_source_bytes(&path)?;
        let uri = path.to_string_lossy().replace('\\', "/");
        engine.session_mut().add_document(&uri, &source).ok()?;
    }
    engine.class_def(qualified).map(Arc::new)
}

/// Non-blocking variant of [`peek_or_load_msl_class`] — returns the
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
    let mut engine = msl_engine().lock().ok()?;
    if !engine.has_class(qualified) {
        return None;
    }
    engine.class_def(qualified).map(Arc::new)
}
