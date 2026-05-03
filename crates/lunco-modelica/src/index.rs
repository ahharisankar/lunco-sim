//! Per-document UI projection.
//!
//! [`ModelicaIndex`] is what panels read. Built once per parse-success from
//! the rumoca AST, then *patched* directly by typed ops for sub-frame
//! interactivity. UI never touches the AST and never runs regex against
//! source text.
//!
//! ## Lifecycle
//!
//! - **Open**: parse via rumoca → build a fresh Index from the AST.
//! - **Edit**: typed op → `patch_*` mutates Index in-place. Panels rerender
//!   next frame. Source text + AST are eventually-consistent (debounced
//!   reparse reconciles any drift).
//! - **Reparse-success**: a new AST arrives → diff against current Index,
//!   apply structural deltas (preserves UI state like selection/zoom).
//!
//! ## What lives here vs in the AST
//!
//! AST = canonical, parser-shaped (rumoca structures, raw `Expression`
//! annotations). Index = UI-shaped: pre-extracted Placement structs,
//! component-keyed connection lookups, BBox caches, anything panels need
//! to render without traversal.

use lunco_doc::{NodeId, TextRange};
use std::collections::HashMap;

// TODO(slotmap): when we hit the perf wall on dense Vec<X> + HashMap<Name, idx>
// patching, swap to slotmap::SlotMap<Key, Entry> for stable handles across
// removes. Keeping plain Vec/HashMap until then so this file has zero new
// dep cost vs current crate.

/// Opaque handle to a component within the Index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentKey(pub u32);

/// Opaque handle to a connection within the Index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionKey(pub u32);

/// Per-document projection that UI consumes.
#[derive(Debug, Default)]
pub struct ModelicaIndex {
    /// Bumped on every patch. Panels can fingerprint to skip rerender.
    pub generation: u64,

    /// The current authoritative source text (synced after parse-success).
    pub source: String,

    /// Components in the active class. Vec for ordered iteration.
    pub components: Vec<ComponentEntry>,
    pub component_by_name: HashMap<String, ComponentKey>,

    /// Connections in the active class.
    pub connections: Vec<ConnectionEntry>,

    /// All classes defined in this document, by qualified name.
    pub classes: HashMap<String, ClassEntry>,

    /// Within-clause path, if any (e.g. `"Modelica.Mechanics"`).
    pub within_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ComponentEntry {
    pub key: ComponentKey,
    pub node_id: NodeId,
    pub name: String,
    pub type_name: String,
    pub source_range: Option<TextRange>,
    pub placement: Option<Placement>,
    /// Causality: input/output/none.
    pub causality: Causality,
    /// Variability: parameter/constant/discrete/continuous.
    pub variability: Variability,
    /// Optional binding expression, source-text form (right-hand side of `= ...`).
    pub binding: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConnectionEntry {
    pub key: ConnectionKey,
    pub node_id: NodeId,
    pub from: ComponentEndpoint,
    pub to: ComponentEndpoint,
    pub waypoints: Vec<(f32, f32)>,
    pub source_range: Option<TextRange>,
}

#[derive(Debug, Clone)]
pub struct ComponentEndpoint {
    pub component_name: String,
    pub port: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClassEntry {
    pub name: String,
    pub kind: ClassKind,
    pub source_range: Option<TextRange>,
    pub extends: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassKind {
    Model,
    Block,
    Connector,
    Package,
    Function,
    Class,
    Type,
    Record,
    ExpandableConnector,
    Operator,
    OperatorRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Causality {
    #[default]
    None,
    Input,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Variability {
    #[default]
    Continuous,
    Discrete,
    Parameter,
    Constant,
}

/// Pre-extracted Placement annotation. Until rumoca grows a typed Placement,
/// this struct is populated by `lunco-modelica`'s annotation parser.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    pub origin: (f32, f32),
    pub extent: ((f32, f32), (f32, f32)),
    pub rotation: f32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Build / patch / reconcile
// ─────────────────────────────────────────────────────────────────────────────
//
// These are the only entry points UI-side code should use to mutate the
// Index. The rumoca-AST → Index builder lives behind `rebuild_from_ast`;
// optimistic edits go through `patch_*`. Both bump `generation`.
//
// Implementations are intentionally stubbed for this skeleton commit —
// the actual Modelica refactor (kill regex, swap projection) lands as
// follow-up commits. See docs/architecture/REFACTOR_PLAN.md.

impl ModelicaIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Discard everything and rebuild from a fresh rumoca AST.
    /// Call on parse-success.
    ///
    /// TODO(refactor): port the existing AST-extract paths in
    /// `lunco-modelica/src/ast_extract.rs` and the (to-be-deleted) regex
    /// scan in `ui/panels/canvas_projection.rs` to populate this. Keep
    /// `Causality` / `Variability` / `binding` from rumoca's typed fields.
    pub fn rebuild_from_ast(&mut self, _ast: &(), _source: &str) {
        // Stub. Implemented in the modelica refactor commit.
        self.generation = self.generation.saturating_add(1);
    }

    /// Optimistic component-add. Returns the assigned key so callers can
    /// reference it before the authoritative reparse confirms.
    pub fn patch_component_added(&mut self, _entry: ComponentEntry) -> ComponentKey {
        self.generation = self.generation.saturating_add(1);
        // Stub.
        ComponentKey(0)
    }

    pub fn patch_component_removed(&mut self, _key: ComponentKey) {
        self.generation = self.generation.saturating_add(1);
    }

    pub fn patch_placement_changed(&mut self, _key: ComponentKey, _placement: Placement) {
        self.generation = self.generation.saturating_add(1);
    }

    pub fn patch_connection_added(&mut self, _entry: ConnectionEntry) -> ConnectionKey {
        self.generation = self.generation.saturating_add(1);
        ConnectionKey(0)
    }

    pub fn patch_connection_removed(&mut self, _key: ConnectionKey) {
        self.generation = self.generation.saturating_add(1);
    }

    /// Look up a component by its display name within the active class.
    pub fn find_component(&self, name: &str) -> Option<&ComponentEntry> {
        let key = self.component_by_name.get(name)?;
        self.components.iter().find(|c| c.key == *key)
    }
}
