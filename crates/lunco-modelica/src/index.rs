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
use rumoca_session::parsing::ast::{
    self as ast,
    ClassType as AstClassType,
    Causality as AstCausality,
    Variability as AstVariability,
};
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
#[derive(Debug, Default, Clone)]
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
    /// Phase 1 (this commit): components + classes + within. Annotations
    /// (Placement, Icon, Diagram, connection waypoints) are populated by
    /// downstream commits — they live in `crate::annotations` /
    /// `crate::diagram` today and will move to `annotation_parse.rs`
    /// when the placement metamodel lands.
    pub fn rebuild_from_ast(&mut self, ast: &ast::StoredDefinition, source: &str) {
        self.generation = self.generation.saturating_add(1);
        self.source = source.to_string();
        self.components.clear();
        self.component_by_name.clear();
        self.connections.clear();
        self.classes.clear();
        self.within_path = ast
            .within
            .as_ref()
            .map(|n| format!("{}", n));

        // Walk top-level classes; nested classes go into their own
        // entry so panels can drill in by qualified name.
        for (qualified, class_def) in &ast.classes {
            insert_class_recursive(self, qualified.clone(), class_def);
        }
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

// ─────────────────────────────────────────────────────────────────────────────
// AST → Index helpers
// ─────────────────────────────────────────────────────────────────────────────

fn insert_class_recursive(idx: &mut ModelicaIndex, qualified: String, class_def: &ast::ClassDef) {
    let entry = ClassEntry {
        name: qualified.clone(),
        kind: map_class_type(&class_def.class_type),
        source_range: Some(TextRange::new(
            class_def.location.start as usize,
            class_def.location.end as usize,
        )),
        extends: class_def
            .extends
            .iter()
            .map(|e| format!("{}", e.base_name))
            .collect(),
    };
    idx.classes.insert(qualified.clone(), entry);

    // Components — populate the flat component list. Today's Index
    // collapses all classes' components into one list (callers filter
    // by class via NodeId or class scope). Panels that scope by class
    // do so via the qualified-name prefix on NodeId.
    for (name, comp) in class_def.iter_components() {
        let key = ComponentKey(idx.components.len() as u32);
        let entry = ComponentEntry {
            key,
            node_id: NodeId::new(format!("{}|component|{}", qualified, name)),
            name: name.to_string(),
            type_name: format!("{}", comp.type_name),
            source_range: Some(TextRange::new(
                comp.name_token.location.start as usize,
                comp.name_token.location.end as usize,
            )),
            placement: None, // populated by annotation_parse in a follow-up
            causality: map_causality(&comp.causality),
            variability: map_variability(&comp.variability),
            binding: comp.binding.as_ref().map(|_| String::new()),
            // ^ Placeholder: rumoca's `binding: Option<Expression>` —
            //   reprinting expressions to source text is the printer's
            //   job; until then we just record presence.
        };
        idx.component_by_name.insert(name.to_string(), key);
        idx.components.push(entry);
    }

    // Recurse nested classes (e.g. examples inside a package).
    for (nested_name, nested_def) in class_def.iter_classes() {
        let nested_qualified = format!("{}.{}", qualified, nested_name);
        insert_class_recursive(idx, nested_qualified, nested_def);
    }
}

fn map_class_type(t: &AstClassType) -> ClassKind {
    match t {
        AstClassType::Model => ClassKind::Model,
        AstClassType::Class => ClassKind::Class,
        AstClassType::Block => ClassKind::Block,
        AstClassType::Connector => ClassKind::Connector,
        AstClassType::Record => ClassKind::Record,
        AstClassType::Type => ClassKind::Type,
        AstClassType::Package => ClassKind::Package,
        AstClassType::Function => ClassKind::Function,
        AstClassType::Operator => ClassKind::Operator,
    }
}

fn map_causality(c: &AstCausality) -> Causality {
    match c {
        AstCausality::Empty => Causality::None,
        AstCausality::Input(_) => Causality::Input,
        AstCausality::Output(_) => Causality::Output,
    }
}

fn map_variability(v: &AstVariability) -> Variability {
    match v {
        AstVariability::Empty => Variability::Continuous,
        AstVariability::Constant(_) => Variability::Constant,
        AstVariability::Discrete(_) => Variability::Discrete,
        AstVariability::Parameter(_) => Variability::Parameter,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "within Demo;\n\nmodel RC\n  parameter Real R = 100;\n  Real x;\n  Modelica.Electrical.Analog.Basic.Resistor resistor;\nend RC;\n";

    #[test]
    fn rebuild_populates_within_classes_components() {
        let ast = rumoca_phase_parse::parse_to_ast(SRC, "RC.mo").expect("parses");
        let mut idx = ModelicaIndex::new();
        idx.rebuild_from_ast(&ast, SRC);

        assert_eq!(idx.within_path.as_deref(), Some("Demo"));
        assert!(idx.classes.contains_key("RC"), "classes: {:?}", idx.classes.keys().collect::<Vec<_>>());
        assert_eq!(idx.classes["RC"].kind, ClassKind::Model);

        // Three components: R (parameter), x, resistor.
        assert_eq!(idx.components.len(), 3, "components: {:?}", idx.components.iter().map(|c| &c.name).collect::<Vec<_>>());

        let r = idx.find_component("R").expect("R present");
        assert_eq!(r.variability, Variability::Parameter);
        assert_eq!(r.type_name, "Real");

        let resistor = idx.find_component("resistor").expect("resistor present");
        assert_eq!(resistor.type_name, "Modelica.Electrical.Analog.Basic.Resistor");
        assert_eq!(resistor.causality, Causality::None);
    }

    #[test]
    fn rebuild_clears_old_state() {
        let mut idx = ModelicaIndex::new();
        let ast1 = rumoca_phase_parse::parse_to_ast(SRC, "RC.mo").expect("parses");
        idx.rebuild_from_ast(&ast1, SRC);
        let gen_before = idx.generation;

        let small = "model Tiny\nend Tiny;\n";
        let ast2 = rumoca_phase_parse::parse_to_ast(small, "Tiny.mo").expect("parses");
        idx.rebuild_from_ast(&ast2, small);

        assert!(idx.generation > gen_before, "generation must advance");
        assert_eq!(idx.components.len(), 0);
        assert!(idx.classes.contains_key("Tiny"));
        assert!(!idx.classes.contains_key("RC"));
        assert_eq!(idx.within_path, None);
    }
}
