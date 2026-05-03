//! Per-Twin Modelica domain engine.
//!
//! Wraps a long-lived [`rumoca_session::Session`] populated with the
//! source of every open Modelica document in the active Twin.
//! Cross-file queries (inheritance-merged components, name resolution,
//! completion) read from the session's fingerprinted phase caches
//! instead of running their own AST walkers in lunco-modelica.
//!
//! ## Where this fits architecturally
//!
//! - **`lunco-twin`** stays domain-agnostic — it doesn't import rumoca.
//! - **`lunco-doc::DomainEngine`** is the trait Twin/UI talks through.
//! - **`ModelicaEngine`** (this file) is the Modelica-specific impl
//!   that owns rumoca state. Per-Twin in scope; today there's a single
//!   instance because the workbench hosts a single Twin.
//!
//! When multi-Twin lands, this resource becomes
//! `Map<TwinId, ModelicaEngine>` and the trait dispatch routes
//! (twin_id, doc_id) to the right engine. The internal API stays the
//! same.
//!
//! ## What's wired today
//!
//! - [`Self::upsert_document`] / [`Self::close_document`] — add or
//!   replace a document's source in the session.
//! - [`Self::inherited_components`] — calls
//!   `Session::class_component_members_query` so panels get
//!   inheritance-merged member lists for free (no per-panel
//!   reimplementation of `extract_*_inherited`).
//!
//! ## What's deferred (next commits)
//!
//! - Auto-sync system: a Bevy `Update` system that mirrors changes
//!   from `ModelicaDocumentRegistry` into the session. Today callers
//!   call `upsert_document` explicitly.
//! - Rumoca ask: `class_inherited_annotations_query` so Icon/Diagram
//!   merging also goes through the session instead of
//!   `extract_icon_inherited`.
//! - Library-parent session for MSL (`Session::with_library_parent`)
//!   so cross-Twin MSL state is shared once multi-Twin lands.

use lunco_doc::DocumentId;
use rumoca_session::Session;
use std::collections::HashMap;

/// Workspace-wide rumoca state for one Twin's Modelica content.
///
/// Plain Rust — **not** a Bevy `Resource`. Bevy users wrap this in
/// [`ModelicaEngineRes`] (below) which is the actual `Resource`.
/// The split keeps the engine usable from headless contexts
/// (`lunco-twin-server`, CLI, AI-agent runtimes, WASM thin clients)
/// without forcing Bevy into the dependency graph of every consumer.
///
/// Holds a single [`rumoca_session::Session`] populated with the
/// source of every open Modelica document; cross-file queries route
/// through the session's caches.
pub struct ModelicaEngine {
    session: Session,
    /// `DocumentId` → URI used inside the session. Stable for the
    /// document's lifetime; freed on [`Self::close_document`].
    uri_for_doc: HashMap<DocumentId, String>,
}

impl Default for ModelicaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelicaEngine {
    pub fn new() -> Self {
        Self {
            session: Session::default(),
            uri_for_doc: HashMap::new(),
        }
    }

    /// URI we use for `doc_id` inside the session. Untitled / on-disk
    /// docs share the same naming scheme so cross-doc references
    /// work uniformly.
    fn uri(&self, doc_id: DocumentId) -> String {
        format!("doc-{}.mo", doc_id.raw())
    }

    /// Add or update a document's source in the session.
    ///
    /// Both add and update funnel through `Session::add_document` —
    /// rumoca's fingerprint cache invalidates only the affected
    /// per-file phases so subsequent edits are cheap.
    pub fn upsert_document(&mut self, doc_id: DocumentId, source: &str) -> Result<(), String> {
        let uri = self.uri(doc_id);
        self.uri_for_doc.entry(doc_id).or_insert_with(|| uri.clone());
        self.session
            .add_document(&uri, source)
            .map_err(|e| e.to_string())
    }

    /// Forget a document. The current rumoca public API has no
    /// remove-document hook, so the session retains the previous
    /// content until something else overwrites the URI; that's
    /// benign for cross-doc queries (the URI is unique per
    /// `DocumentId` and `DocumentId`s aren't recycled). The map
    /// entry is dropped so reopening the same id starts fresh.
    pub fn close_document(&mut self, doc_id: DocumentId) {
        self.uri_for_doc.remove(&doc_id);
        // TODO(rumoca): public Session::remove_document(uri).
    }

    /// Inheritance-merged component members for a fully-qualified
    /// class. Returns `(name, type)` pairs walking the `extends`
    /// chain — including across files when the bases are in other
    /// open documents.
    ///
    /// This is the call panels SHOULD make instead of running their
    /// own `extract_*_inherited` walker. Cached inside the session
    /// (per [`rumoca_session::Session::class_component_members_query`]).
    pub fn inherited_components(&mut self, qualified: &str) -> Vec<(String, String)> {
        self.session.class_component_members_query(qualified)
    }

    /// Read-only access to the underlying session for advanced queries
    /// not yet wrapped here. Use sparingly — prefer growing this
    /// crate's API over leaking the session through panels.
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Bevy adapter
// ─────────────────────────────────────────────────────────────────────────────
//
// Below: thin `Resource` wrapper + plugin so Bevy users get the
// usual `ResMut<ModelicaEngineRes>` ergonomics. Everything above is
// plain Rust and reusable without Bevy.

use bevy::prelude::*;

/// Bevy `Resource` adapter wrapping a [`ModelicaEngine`].
///
/// Systems take `ResMut<ModelicaEngineRes>` and call engine methods
/// through `Deref`/`DerefMut`:
/// ```ignore
/// fn my_system(mut engine: ResMut<ModelicaEngineRes>) {
///     let members = engine.inherited_components("Vehicle.Engine");
/// }
/// ```
#[derive(Resource, Default, Deref, DerefMut)]
pub struct ModelicaEngineRes(pub ModelicaEngine);

/// Plugin that registers [`ModelicaEngineRes`] in the Bevy world.
///
/// Today added explicitly by tests and by the integration that
/// wants engine-backed inheritance queries. Once the auto-sync
/// system lands this plugin will also schedule it.
pub struct ModelicaEnginePlugin;

impl Plugin for ModelicaEnginePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ModelicaEngineRes>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_components_walks_extends_across_docs() {
        let mut engine = ModelicaEngine::new();
        let base = "model Base\n  Real x;\n  Real y;\nend Base;\n";
        let derived = "model Derived\n  extends Base;\n  Real z;\nend Derived;\n";
        engine
            .upsert_document(DocumentId::new(1), base)
            .expect("base parses");
        engine
            .upsert_document(DocumentId::new(2), derived)
            .expect("derived parses");

        let members = engine.inherited_components("Derived");
        let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"x") && names.contains(&"y"),
            "expected inherited x + y, got {names:?}"
        );
        assert!(names.contains(&"z"), "expected own z, got {names:?}");
    }

    #[test]
    fn upsert_overwrites_previous_source() {
        let mut engine = ModelicaEngine::new();
        let v1 = "model M\n  Real a;\nend M;\n";
        let v2 = "model M\n  Real a;\n  Real b;\nend M;\n";
        engine.upsert_document(DocumentId::new(1), v1).unwrap();
        let n1 = engine.inherited_components("M").len();
        engine.upsert_document(DocumentId::new(1), v2).unwrap();
        let n2 = engine.inherited_components("M").len();
        assert!(n2 > n1, "second upsert should replace v1; n1={n1}, n2={n2}");
    }

    #[test]
    fn close_document_drops_uri_mapping() {
        let mut engine = ModelicaEngine::new();
        engine
            .upsert_document(DocumentId::new(1), "model M\nend M;\n")
            .unwrap();
        assert!(engine.uri_for_doc.contains_key(&DocumentId::new(1)));
        engine.close_document(DocumentId::new(1));
        assert!(!engine.uri_for_doc.contains_key(&DocumentId::new(1)));
    }
}
