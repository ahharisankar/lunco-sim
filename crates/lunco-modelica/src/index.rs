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

use crate::pretty::Placement;
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
///
/// Patched optimistically by `apply_patch` (see [`crate::document::ModelicaDocument`])
/// in response to structural [`crate::document::ModelicaChange`] events, so panels
/// see edits in the same frame. Reconciled to the AST on every parse-success.
#[derive(Debug, Default, Clone)]
pub struct ModelicaIndex {
    /// Bumped on every patch. Panels can fingerprint to skip rerender.
    pub generation: u64,

    /// The current authoritative source text (synced after parse-success).
    pub source: String,

    /// All component entries across every class, in arbitrary order.
    pub components: Vec<ComponentEntry>,

    /// `(qualified_class, instance_name)` → key.
    pub component_by_qualified: HashMap<(String, String), ComponentKey>,

    /// `qualified_class` → ordered keys (declaration order).
    pub components_by_class: HashMap<String, Vec<ComponentKey>>,

    /// Connections in arbitrary order.
    pub connections: Vec<ConnectionEntry>,

    /// `qualified_class` → ordered keys.
    pub connections_by_class: HashMap<String, Vec<ConnectionKey>>,

    /// All classes defined in this document, by qualified name.
    pub classes: HashMap<String, ClassEntry>,

    /// Within-clause path, if any (e.g. `"Modelica.Mechanics"`).
    pub within_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ComponentEntry {
    pub key: ComponentKey,
    pub node_id: NodeId,
    /// Qualified class this component belongs to (e.g. `"RC_Circuit"`).
    pub class: String,
    pub name: String,
    pub type_name: String,
    /// Description string from the declaration (e.g. `"Resistance"` in
    /// `Real R "Resistance";`). Empty when none was provided.
    pub description: String,
    /// Modifications attached to the declaration
    /// (e.g. `{"min": "0", "max": "100"}` for `Real x(min=0, max=100)`).
    pub modifications: HashMap<String, String>,
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
    /// Description string from the class header
    /// (`model X "description"`). Empty when none was authored.
    pub description: String,
    /// Qualified names of nested classes declared inside this one,
    /// in declaration order. Used for tree assembly in browsers.
    pub children: Vec<String>,
    /// Authored Icon annotation, if present. Populated from
    /// [`crate::annotations::extract_icon`] during rebuild.
    pub icon: Option<crate::annotations::Icon>,
    /// `(info, revisions)` from the class's `Documentation(...)`
    /// annotation. Both are `None` when no documentation was authored.
    pub documentation: (Option<String>, Option<String>),
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

// Placement is re-exported from `crate::pretty::Placement` to keep the
// Index in lockstep with the wire / change-event format. UI panels read
// `entry.placement: Option<Placement>` directly.

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
        self.component_by_qualified.clear();
        self.components_by_class.clear();
        self.connections.clear();
        self.connections_by_class.clear();
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

    /// Optimistic component-add. Called from
    /// [`crate::document::ModelicaDocument::apply_patch`] in response
    /// to a [`crate::document::ModelicaChange::ComponentAdded`].
    /// Returns the assigned key.
    ///
    /// `class` is fully qualified (e.g. `"Rocket.Engine"`). The Index
    /// stores a placeholder entry with no source range / placement —
    /// those fill in on the next AST reconcile.
    pub fn patch_component_added(&mut self, class: &str, name: &str, type_name: &str) -> ComponentKey {
        self.generation = self.generation.saturating_add(1);
        let key = ComponentKey(self.components.len() as u32);
        let entry = ComponentEntry {
            key,
            node_id: NodeId::new(format!("{}|component|{}", class, name)),
            class: class.to_string(),
            name: name.to_string(),
            type_name: type_name.to_string(),
            description: String::new(),
            modifications: HashMap::new(),
            source_range: None,
            placement: None,
            causality: Causality::None,
            variability: Variability::Continuous,
            binding: None,
        };
        self.component_by_qualified
            .insert((class.to_string(), name.to_string()), key);
        self.components_by_class
            .entry(class.to_string())
            .or_default()
            .push(key);
        self.components.push(entry);
        key
    }

    /// Optimistic component-remove. No-op when not present (the apply
    /// pipeline guarantees the change events match reality, but the
    /// reconcile-on-reparse path makes a stale call benign).
    pub fn patch_component_removed(&mut self, class: &str, name: &str) {
        self.generation = self.generation.saturating_add(1);
        let qualified = (class.to_string(), name.to_string());
        let Some(key) = self.component_by_qualified.remove(&qualified) else {
            return;
        };
        if let Some(list) = self.components_by_class.get_mut(class) {
            list.retain(|k| *k != key);
        }
        if let Some(pos) = self.components.iter().position(|c| c.key == key) {
            self.components.remove(pos);
        }
    }

    /// Optimistic placement-set. No-op if the (class, name) doesn't
    /// resolve in the Index (lazy reconcile-on-reparse will re-sync).
    pub fn patch_placement_changed(&mut self, class: &str, name: &str, placement: Placement) {
        self.generation = self.generation.saturating_add(1);
        let qualified = (class.to_string(), name.to_string());
        let Some(key) = self.component_by_qualified.get(&qualified).copied() else {
            return;
        };
        if let Some(entry) = self.components.iter_mut().find(|c| c.key == key) {
            entry.placement = Some(placement);
        }
    }

    /// Look up a component by `(class, name)`. Returns `None` if the
    /// component doesn't exist.
    pub fn find_component(&self, class: &str, name: &str) -> Option<&ComponentEntry> {
        let key = self
            .component_by_qualified
            .get(&(class.to_string(), name.to_string()))
            .copied()?;
        self.components.iter().find(|c| c.key == key)
    }

    /// Iterate components in `class` in declaration order.
    pub fn components_in_class<'a>(
        &'a self,
        class: &str,
    ) -> impl Iterator<Item = &'a ComponentEntry> + 'a {
        self.components_by_class
            .get(class)
            .into_iter()
            .flat_map(|keys| keys.iter())
            .filter_map(move |key| self.components.iter().find(|c| c.key == *key))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AST → Index helpers
// ─────────────────────────────────────────────────────────────────────────────

fn insert_class_recursive(idx: &mut ModelicaIndex, qualified: String, class_def: &ast::ClassDef) {
    // Description from the class header (`model X "desc"`).
    let description = class_def
        .description
        .iter()
        .next()
        .map(|t| t.text.as_ref().trim_matches('"').to_string())
        .unwrap_or_default();

    // Direct child class qualified names — rebuild's recursion fills
    // them in below.
    let children: Vec<String> = class_def
        .iter_classes()
        .map(|(name, _)| format!("{}.{}", qualified, name))
        .collect();

    // Annotation extraction reuses the existing helpers so Index stays
    // in lockstep with the model_view / canvas_diagram extractors.
    let icon = crate::annotations::extract_icon(&class_def.annotation);
    let documentation =
        crate::ui::panels::model_view::extract_documentation(&class_def.annotation);

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
        description,
        children,
        icon,
        documentation,
    };
    idx.classes.insert(qualified.clone(), entry);

    // Components — keyed by (class, name) so multiple classes can hold
    // colliding instance names without aliasing. Per-class iteration
    // preserves declaration order via `components_by_class`.
    //
    // Description + modifications come from `ast_extract::extract_components_for_class`
    // which runs the description + modification-expression flattening
    // logic. Calling that public helper keeps Index in lockstep with
    // the inspector's previous direct-AST path.
    let infos = crate::ast_extract::extract_components_for_class(class_def);
    let info_by_name: HashMap<String, crate::ast_extract::ComponentInfo> =
        infos.into_iter().map(|i| (i.name.clone(), i)).collect();
    for (name, comp) in class_def.iter_components() {
        let key = ComponentKey(idx.components.len() as u32);
        let info = info_by_name.get(name).cloned();
        let (description, modifications) = info
            .map(|i| (i.description, i.modifications))
            .unwrap_or_default();
        // Placement extraction reuses the metamodel
        // [`crate::annotations::extract_placement`] and converts the
        // annotation-shaped `Placement(transformation(...))` to the
        // simpler `pretty::Placement` (centre+size) that the wire
        // format uses.
        let placement = crate::annotations::extract_placement(&comp.annotation)
            .map(annotation_placement_to_pretty);
        let entry = ComponentEntry {
            key,
            node_id: NodeId::new(format!("{}|component|{}", qualified, name)),
            class: qualified.clone(),
            name: name.to_string(),
            type_name: format!("{}", comp.type_name),
            description,
            modifications,
            source_range: Some(TextRange::new(
                comp.name_token.location.start as usize,
                comp.name_token.location.end as usize,
            )),
            placement,
            causality: map_causality(&comp.causality),
            variability: map_variability(&comp.variability),
            binding: comp.binding.as_ref().map(|_| String::new()),
            // ^ Placeholder: rumoca's `binding: Option<Expression>` —
            //   reprinting expressions to source text is the printer's
            //   job; until then we just record presence.
        };
        idx.component_by_qualified
            .insert((qualified.clone(), name.to_string()), key);
        idx.components_by_class
            .entry(qualified.clone())
            .or_default()
            .push(key);
        idx.components.push(entry);
    }

    // Recurse nested classes (e.g. examples inside a package).
    for (nested_name, nested_def) in class_def.iter_classes() {
        let nested_qualified = format!("{}.{}", qualified, nested_name);
        insert_class_recursive(idx, nested_qualified, nested_def);
    }
}

fn annotation_placement_to_pretty(p: crate::annotations::Placement) -> Placement {
    let extent = p.transformation.extent;
    let origin = p.transformation.origin;
    let x_min = extent.p1.x.min(extent.p2.x);
    let x_max = extent.p1.x.max(extent.p2.x);
    let y_min = extent.p1.y.min(extent.p2.y);
    let y_max = extent.p1.y.max(extent.p2.y);
    let cx = (x_min + x_max) * 0.5 + origin.x;
    let cy = (y_min + y_max) * 0.5 + origin.y;
    Placement {
        x: cx as f32,
        y: cy as f32,
        width: (x_max - x_min) as f32,
        height: (y_max - y_min) as f32,
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

        let r = idx.find_component("RC", "R").expect("R present");
        assert_eq!(r.variability, Variability::Parameter);
        assert_eq!(r.type_name, "Real");

        let resistor = idx.find_component("RC", "resistor").expect("resistor present");
        assert_eq!(resistor.type_name, "Modelica.Electrical.Analog.Basic.Resistor");
        assert_eq!(resistor.causality, Causality::None);

        // Per-class iterator preserves declaration order.
        let names: Vec<_> = idx
            .components_in_class("RC")
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, vec!["R", "x", "resistor"]);
    }

    #[test]
    fn patch_component_added_then_removed() {
        let mut idx = ModelicaIndex::new();
        let key = idx.patch_component_added("RC", "extra", "Real");
        assert!(idx.find_component("RC", "extra").is_some());
        assert_eq!(idx.find_component("RC", "extra").unwrap().key, key);

        idx.patch_component_removed("RC", "extra");
        assert!(idx.find_component("RC", "extra").is_none());
        assert!(!idx.components_by_class.get("RC").map(|v| !v.is_empty()).unwrap_or(false));
    }

    #[test]
    fn patch_placement_changed_updates_existing() {
        let mut idx = ModelicaIndex::new();
        idx.patch_component_added("RC", "r1", "Resistor");
        let new_placement = Placement::at(10.0, 20.0);
        idx.patch_placement_changed("RC", "r1", new_placement);
        let entry = idx.find_component("RC", "r1").expect("r1 present");
        let p = entry.placement.expect("placement set");
        assert_eq!(p.x, 10.0);
        assert_eq!(p.y, 20.0);
    }

    #[test]
    fn patch_placement_on_missing_component_is_noop() {
        let mut idx = ModelicaIndex::new();
        // Should not panic; should silently ignore.
        idx.patch_placement_changed("RC", "nope", Placement::at(0.0, 0.0));
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
