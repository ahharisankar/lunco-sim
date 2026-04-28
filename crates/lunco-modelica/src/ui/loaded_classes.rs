//! Loaded Modelica top-level classes — the live set of class roots
//! the user has access to in this session.
//!
//! Models OMEdit's "Libraries Browser" surface: a flat list of
//! top-level classes regardless of source. MSL, bundled examples,
//! `twin.toml`-pinned externals, single user files, future remote
//! libraries — they all surface here as siblings.
//!
//! ## Why "class" not "package"
//!
//! Per Modelica 3.6 §13.4, a top-level class is anything in the
//! global namespace that resolves to a fully-qualified name. Most
//! are `package` (libraries), but a single dropped `Resistor.mo`
//! with `model Resistor … end Resistor;` is a top-level **model** —
//! a class but not a package. The umbrella term is **class**;
//! "library" is colloquial and "package" is one specific kind of
//! class.
//!
//! ## Lifecycle
//!
//! - **Defaults** (MSL, ModelicaServices when available) — registered
//!   by `ModelicaUiPlugin::build`, never unloaded.
//! - **Twin externals** — registered on `TwinAdded` after reading
//!   `[modelica] externals` from `twin.toml`. Unregistered on
//!   `TwinClosed`. *(Plumbing deferred — architecture is in place
//!   so the loader slots in later without touching the browser.)*
//! - **Workspace docs** — one [`WorkspaceClass`] per writable /
//!   Untitled Modelica document. Registered on `DocumentOpened`,
//!   dropped on `DocumentClosed`.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_workbench::BrowserCtx;

use crate::ui::state::ModelicaDocumentRegistry;
use lunco_doc::DocumentId;

/// A top-level Modelica class loaded into the current session.
///
/// One trait impl per source kind: [`SystemLibraryClass`] for
/// disk-backed libraries (MSL, externals), [`WorkspaceClass`] for
/// writable / Untitled documents the user is authoring, and any
/// future remote / FMU sources.
pub trait LoadedClass: Send + Sync + 'static {
    /// Stable id used as egui salt and for unregistration when the
    /// underlying source goes away (Twin closed, document closed).
    fn id(&self) -> &str;

    /// Display name shown as the top-level row. `&BrowserCtx` for
    /// dynamic naming — workspace classes show their current
    /// `Untitled`-N or file-stem name; system libraries return a
    /// constant.
    fn name(&self, ctx: &BrowserCtx<'_>) -> String;

    /// Editable? Drives the row's writable badge (workspace vs
    /// library). Read-only system libs render a lock affordance;
    /// drag and Save respect this independently via document-level
    /// origin checks.
    fn writable(&self) -> bool {
        false
    }

    /// Default expand state on first render. Built-in libraries are
    /// closed (huge trees, user expands on demand); workspace
    /// classes default to open (this is what the user is editing).
    fn default_open(&self) -> bool {
        false
    }

    /// Paint the class's children inline at the caller's egui
    /// cursor — the caller has already drawn a `CollapsingHeader`
    /// row for this entry.
    fn render_children(&mut self, ui: &mut egui::Ui, ctx: &mut BrowserCtx<'_>);
}

/// Live registry of [`LoadedClass`] entries. Iterated by
/// `ModelicaSection::render` each frame; mutated by lifecycle
/// observers as Twins / documents come and go.
#[derive(Resource, Default)]
pub struct LoadedModelicaClasses {
    pub entries: Vec<Box<dyn LoadedClass>>,
}

impl LoadedModelicaClasses {
    /// Append a new class. Order is render order.
    pub fn register(&mut self, class: Box<dyn LoadedClass>) {
        self.entries.push(class);
    }

    /// Drop the entry whose [`LoadedClass::id`] matches. Returns
    /// `true` if an entry was removed.
    pub fn unregister(&mut self, id: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|c| c.id() != id);
        before != self.entries.len()
    }
}

// ─────────────────────────────────────────────────────────────────────
// SystemLibraryClass — disk-backed library (MSL, Bundled, externals)
// ─────────────────────────────────────────────────────────────────────

/// A read-only library whose tree lives in
/// [`PackageTreeCache::roots`] under a stable id. Rendering
/// delegates to the existing
/// [`render_root_subtree`](crate::ui::panels::package_browser::render_root_subtree)
/// helper — same lazy disk scan + `render_node` recursion the
/// standalone Package Browser uses.
pub struct SystemLibraryClass {
    /// `cache.roots[*]` id (`"msl_root"`, `"bundled_root"`, ...).
    cache_root_id: String,
    /// Display name shown on the row (`"Modelica"`,
    /// `"Bundled Examples"`, ...).
    display_name: String,
    /// Whether this row is open by default. Big libraries (MSL)
    /// stay closed; short bundled lists open.
    default_open: bool,
}

impl SystemLibraryClass {
    pub fn new(
        cache_root_id: impl Into<String>,
        display_name: impl Into<String>,
        default_open: bool,
    ) -> Self {
        Self {
            cache_root_id: cache_root_id.into(),
            display_name: display_name.into(),
            default_open,
        }
    }
}

impl LoadedClass for SystemLibraryClass {
    fn id(&self) -> &str {
        &self.cache_root_id
    }

    fn name(&self, _ctx: &BrowserCtx<'_>) -> String {
        self.display_name.clone()
    }

    fn writable(&self) -> bool {
        false
    }

    fn default_open(&self) -> bool {
        self.default_open
    }

    fn render_children(&mut self, ui: &mut egui::Ui, ctx: &mut BrowserCtx<'_>) {
        crate::ui::panels::package_browser::render_root_subtree(
            ctx.world,
            ui,
            &self.cache_root_id,
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// WorkspaceClass — one per writable / Untitled Modelica document
// ─────────────────────────────────────────────────────────────────────

/// A writable Modelica document the user is authoring — one
/// `LoadedClass` per document, matching OMEdit's flat layout where
/// `Untitled1`, `MyBalloon`, etc. each appear as siblings of MSL.
///
/// Reads source-of-truth from [`ModelicaDocumentRegistry`]: name,
/// AST, current dirty state. Stateless beyond the doc id.
pub struct WorkspaceClass {
    doc_id: DocumentId,
    cached_id: String,
}

impl WorkspaceClass {
    pub fn new(doc_id: DocumentId) -> Self {
        Self {
            cached_id: format!("workspace:{}", doc_id.raw()),
            doc_id,
        }
    }
}

impl LoadedClass for WorkspaceClass {
    fn id(&self) -> &str {
        &self.cached_id
    }

    fn name(&self, ctx: &BrowserCtx<'_>) -> String {
        ctx.world
            .get_resource::<ModelicaDocumentRegistry>()
            .and_then(|reg| reg.host(self.doc_id))
            .map(|host| host.document().origin().display_name())
            .unwrap_or_else(|| "(closed)".to_string())
    }

    fn writable(&self) -> bool {
        true
    }

    fn default_open(&self) -> bool {
        // Workspace items are what the user is actively editing —
        // expand by default so the class structure is one click away.
        true
    }

    fn render_children(&mut self, ui: &mut egui::Ui, ctx: &mut BrowserCtx<'_>) {
        crate::ui::browser_section::render_workspace_doc(ui, ctx, self.doc_id);
    }
}
