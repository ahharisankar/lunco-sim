//! Structural AST mutation helpers — the building blocks for the
//! AST-canonical migration in `docs/architecture/TAB_AST_ROADMAP.md`
//! Section A.
//!
//! ## Why this module exists
//!
//! Today every `ModelicaOp` compiles to a `(byte_range, replacement)`
//! patch via `pretty::*` text emitters and source-byte scans inside
//! `document::op_to_patch`. The migration replaces that path with:
//!
//! 1. clone the parsed `StoredDefinition`,
//! 2. mutate the relevant `ClassDef` via a helper from this module,
//! 3. emit the mutated class via `class_def.to_modelica(indent)`,
//! 4. produce a `(range, replacement)` patch by diffing the new class
//!    text against the slice of source covering the original class.
//!
//! Step 2 is what lives here. Steps 1, 3, 4 land in `op_to_patch`
//! itself once enough helpers exist that we can flip ops over.
//!
//! ## Layering
//!
//! Per `AGENTS.md` §4.1, this is Layer-2 domain logic. **Headless** —
//! no Bevy, no UI, pure functions on rumoca AST nodes. Tests live in
//! `crates/lunco-modelica/tests/ast_mut_*.rs` and run with no
//! workbench / renderer plugins.
//!
//! ## Scope today
//!
//! Batch 1 of A.2: [`set_parameter`]. Smallest blast radius — modifies
//! one entry in one component's `modifications: IndexMap<String,
//! Expression>`. No topology change, no equation reordering. The
//! pattern established here generalises to the rest of the helpers in
//! batches 2 and 3.
//!
//! `set_placement` lands next session: needs an annotation-tree edit,
//! denser than `set_parameter`.

use std::ops::Range;
use std::sync::Arc;

use rumoca_session::parsing::ast::{ClassDef, Expression, StoredDefinition, Token, TerminalType};
use rumoca_phase_parse::parse_to_ast;

use crate::pretty;

/// Errors from structural AST mutation. Stays small on purpose —
/// callers (e.g. `op_to_patch`) translate these into `DocumentError`.
#[derive(Debug, Clone, PartialEq)]
pub enum AstMutError {
    /// Target class is not in the parsed `StoredDefinition`. Names use
    /// dotted form (`"Foo.Bar.Baz"`) and resolve top-down through nested
    /// `classes` maps.
    ClassNotFound(String),
    /// Component name not present in `class.components`. Often a stale
    /// op against an out-of-date AST snapshot.
    ComponentNotFound {
        /// Class the component was looked up in.
        class: String,
        /// Component name that was missing.
        component: String,
    },
    /// Failed to parse a value fragment into an [`Expression`]. Carries
    /// the offending source text to make UI surfacing easy.
    ValueParseFailed {
        /// The offending value text.
        value: String,
    },
    /// `add_component` was called with a component name that already
    /// exists. Adding a duplicate would silently shadow the existing
    /// declaration in `components: IndexMap`, which is rarely the
    /// caller's intent — surface explicitly so they can decide
    /// (remove-then-add for type changes, `set_parameter` for
    /// modification updates).
    DuplicateComponent {
        /// Class the component was being inserted into.
        class: String,
        /// Component name that already exists.
        component: String,
    },
    /// `add_class` was called with a class name that already exists in
    /// the target parent (or top level). Same rationale as
    /// [`Self::DuplicateComponent`].
    DuplicateClass {
        /// Parent class qualified name, or `"(top-level)"` for the
        /// `StoredDefinition.classes` root.
        parent: String,
        /// Class name that already exists.
        name: String,
    },
    /// `remove_connection` did not find a matching `connect(from, to)`
    /// equation. Direction-sensitive: the canvas emits canonical
    /// direction, so this isn't expected to false-positive in
    /// practice; if it does we'll widen to direction-insensitive match.
    ConnectionNotFound {
        /// Class whose equations were searched.
        class: String,
        /// `component.port` form of the missing source endpoint.
        from: String,
        /// `component.port` form of the missing target endpoint.
        to: String,
    },
}

impl std::fmt::Display for AstMutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AstMutError::ClassNotFound(name) => write!(f, "class not found: {name}"),
            AstMutError::ComponentNotFound { class, component } => {
                write!(f, "component `{component}` not found in class `{class}`")
            }
            AstMutError::ValueParseFailed { value } => {
                write!(f, "could not parse value `{value}` as a Modelica expression")
            }
            AstMutError::DuplicateComponent { class, component } => write!(
                f,
                "component `{component}` already exists in class `{class}`"
            ),
            AstMutError::DuplicateClass { parent, name } => write!(
                f,
                "class `{name}` already exists under `{parent}`"
            ),
            AstMutError::ConnectionNotFound { class, from, to } => write!(
                f,
                "connection `connect({from}, {to})` not found in class `{class}`"
            ),
        }
    }
}

impl std::error::Error for AstMutError {}

/// Resolve a dotted-qualified class path against a parsed
/// `StoredDefinition`. `"Foo"` looks up at the top level; `"Foo.Bar"`
/// descends into `classes["Foo"].classes["Bar"]`.
///
/// Mutable variant — callers hold a clone of the AST and mutate it
/// before emitting source. Used by every helper in this module that
/// takes a class path string.
pub fn lookup_class_mut<'a>(
    sd: &'a mut StoredDefinition,
    qualified: &str,
) -> Result<&'a mut ClassDef, AstMutError> {
    // Empty path is the root namespace, which we don't model as a class
    // — bail early so callers don't pass it accidentally and end up
    // with a confusing "class `` not found" message.
    if qualified.is_empty() {
        return Err(AstMutError::ClassNotFound(qualified.into()));
    }
    let mut parts = qualified.split('.');
    let head = parts.next().expect("split always yields at least one piece");
    let mut current = sd
        .classes
        .get_mut(head)
        .ok_or_else(|| AstMutError::ClassNotFound(qualified.to_string()))?;
    for part in parts {
        current = current
            .classes
            .get_mut(part)
            .ok_or_else(|| AstMutError::ClassNotFound(qualified.to_string()))?;
    }
    Ok(current)
}

/// Set or replace a single parameter modification on a component.
///
/// Mirrors `ModelicaOp::SetParameter`: locates `component` inside
/// `class`, parses `value_text` into an [`Expression`], and routes it
/// to the right field on the `Component`.
///
/// **Why this is more than a single `IndexMap::insert`:** rumoca's
/// `Component` lifts a few specially-treated modifier names out of the
/// generic `modifications: IndexMap<String, Expression>` map and into
/// dedicated typed fields (`start`, `binding`, …). Writing to the map
/// when one of these dedicated fields is also populated causes
/// `to_modelica()` to emit *both* — `Real k(start = 2)(start = 0.5)`
/// — which then fails to reparse. The mapping below routes each known
/// special-case attribute to its dedicated field; everything else goes
/// through the `modifications` map.
///
/// Today we route only `start` because it's the only attribute the
/// canvas/inspector edit on the existing op surface. Add a row when a
/// new dedicated field shows up in `Component`.
///
/// **Why parse instead of build the `Expression` by hand:** rumoca's
/// `Expression` enum has dozens of variants and the value text comes
/// from UI code (inspector input, API call) that the user expects to
/// be Modelica-shaped — `"1.5"`, `"if cond then a else b"`, `"{1, 2,
/// 3}"`. Hand-rolling a parser sized to the inspector inputs is more
/// brittle than reusing rumoca's. The cost is a single `parse_to_ast`
/// call per `SetParameter` (small fragment, fast), shared with the
/// future `set_placement` helper.
pub fn set_parameter(
    class: &mut ClassDef,
    component: &str,
    param: &str,
    value_text: &str,
) -> Result<(), AstMutError> {
    // Capture the class name before the mutable borrow of `components`
    // so error construction below doesn't fight the borrow checker.
    let class_name = class.name.text.to_string();
    let comp = class
        .components
        .get_mut(component)
        .ok_or_else(|| AstMutError::ComponentNotFound {
            class: class_name,
            component: component.to_string(),
        })?;
    let expr = parse_value_fragment(value_text)?;
    match param {
        "start" => {
            comp.start = expr;
            comp.start_is_modification = true;
        }
        _ => {
            comp.modifications.insert(param.to_string(), expr);
        }
    }
    Ok(())
}

/// Append a new component to a class.
///
/// Mirrors `ModelicaOp::AddComponent`. The new component is constructed
/// by rendering `decl` into a stub-class source fragment via
/// `pretty::component_decl` and parsing it back to AST — same trick as
/// [`set_parameter`]'s `parse_value_fragment`. Errors out if a
/// component with the same name already exists; replacing in place is
/// a different operation (`SetParameter` for individual modifications,
/// remove-then-add for type changes).
pub fn add_component(
    class: &mut ClassDef,
    decl: &pretty::ComponentDecl,
) -> Result<(), AstMutError> {
    if class.components.contains_key(&decl.name) {
        return Err(AstMutError::DuplicateComponent {
            class: class.name.text.to_string(),
            component: decl.name.clone(),
        });
    }
    let new_component = parse_component_fragment(decl)?;
    class.components.insert(decl.name.clone(), new_component);
    Ok(())
}

/// Remove a component by name. Returns `ComponentNotFound` if absent —
/// the caller decides whether stale ops on a removed component should
/// be silent (idempotent) or surfaced.
pub fn remove_component(class: &mut ClassDef, name: &str) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    if class.components.shift_remove(name).is_none() {
        return Err(AstMutError::ComponentNotFound {
            class: class_name,
            component: name.to_string(),
        });
    }
    Ok(())
}

/// Append a `connect(...)` equation to a class.
///
/// Mirrors `ModelicaOp::AddConnection`. Uses the same parse-fragment
/// trick: render the `ConnectEquation` via `pretty::connect_equation`,
/// wrap in a stub class equation section, parse, and lift the
/// resulting `Equation::Connect` into the target's equations list.
pub fn add_connection(
    class: &mut ClassDef,
    eq: &pretty::ConnectEquation,
) -> Result<(), AstMutError> {
    let new_eq = parse_connect_equation_fragment(eq)?;
    class.equations.push(new_eq);
    Ok(())
}

/// Remove a `connect(...)` equation matching `(from, to)` PortRefs.
/// Returns `ConnectionNotFound` when no match exists. Direction is
/// matched as written: `connect(a.p, b.q)` and `connect(b.q, a.p)` are
/// distinct from this helper's perspective. (Modelica's connection
/// semantics treat them as equivalent, but the canvas always emits a
/// canonical direction so direction-sensitive matching is sufficient
/// for canvas-driven edits and avoids false matches against unrelated
/// connections.)
pub fn remove_connection(
    class: &mut ClassDef,
    from: &pretty::PortRef,
    to: &pretty::PortRef,
) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let before = class.equations.len();
    class.equations.retain(|eq| {
        !matches!(
            eq,
            rumoca_session::parsing::ast::Equation::Connect { lhs, rhs, .. }
                if matches_port_ref(lhs, from) && matches_port_ref(rhs, to)
        )
    });
    if class.equations.len() == before {
        return Err(AstMutError::ConnectionNotFound {
            class: class_name,
            from: format!("{}.{}", from.component, from.port),
            to: format!("{}.{}", to.component, to.port),
        });
    }
    Ok(())
}

/// Match a parsed `ComponentReference` against a `pretty::PortRef`.
///
/// Two shapes show up in practice:
/// - `connect(a.p, b.q)` — PortRef has both `component` and `port`,
///   reference is a two-segment `a.p`.
/// - `connect(a, b)` — for top-level connector instances where the
///   whole component IS a port. PortRef leaves `port` empty,
///   reference is a single-segment `a`.
///
/// Anything else (deeper paths, subscripts) is rejected as
/// canvas-impossible.
fn matches_port_ref(
    cref: &rumoca_session::parsing::ast::ComponentReference,
    port: &pretty::PortRef,
) -> bool {
    if port.port.is_empty() {
        cref.parts.len() == 1 && &*cref.parts[0].ident.text == port.component
    } else {
        cref.parts.len() == 2
            && &*cref.parts[0].ident.text == port.component
            && &*cref.parts[1].ident.text == port.port
    }
}

/// Parse a `pretty::ComponentDecl` into a rumoca [`Component`] by
/// wrapping it in a stub class. Returns the lifted Component ready to
/// insert into a target ClassDef's `components` map.
fn parse_component_fragment(
    decl: &pretty::ComponentDecl,
) -> Result<rumoca_session::parsing::ast::Component, AstMutError> {
    let body = pretty::component_decl(decl);
    let stub = format!("model __LunCoFragment\n{body}end __LunCoFragment;\n");
    let parsed = parse_to_ast(&stub, "__lunco_fragment.mo").map_err(|_| {
        AstMutError::ValueParseFailed { value: body.clone() }
    })?;
    let class = parsed.classes.get("__LunCoFragment").ok_or_else(|| {
        AstMutError::ValueParseFailed { value: body.clone() }
    })?;
    class
        .components
        .get(&decl.name)
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: body })
}

/// Parse a `pretty::ConnectEquation` into a rumoca `Equation::Connect`.
///
/// Built directly from the typed `PortRef` fields rather than going
/// through `pretty::connect_equation`, which always emits
/// `component.port` and produces an invalid `a.` fragment when `port`
/// is empty (used for top-level connector instances). When `pretty/`
/// is deleted in A.4 this becomes the only emitter for connect
/// equations.
fn parse_connect_equation_fragment(
    eq: &pretty::ConnectEquation,
) -> Result<rumoca_session::parsing::ast::Equation, AstMutError> {
    let from_text = render_port_ref(&eq.from);
    let to_text = render_port_ref(&eq.to);
    let body = format!("  connect({from_text}, {to_text});\n");
    let stub = format!("model __LunCoFragment\nequation\n{body}end __LunCoFragment;\n");
    let parsed = parse_to_ast(&stub, "__lunco_fragment.mo").map_err(|_| {
        AstMutError::ValueParseFailed { value: body.clone() }
    })?;
    let class = parsed.classes.get("__LunCoFragment").ok_or_else(|| {
        AstMutError::ValueParseFailed { value: body.clone() }
    })?;
    class
        .equations
        .first()
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: body })
}

/// Render a [`pretty::PortRef`] into its source form. Two-segment
/// `component.port` when both fields are populated; just `component`
/// when `port` is empty (top-level connector instance).
fn render_port_ref(p: &pretty::PortRef) -> String {
    if p.port.is_empty() {
        p.component.clone()
    } else {
        format!("{}.{}", p.component, p.port)
    }
}

/// Add a new variable declaration to a class.
///
/// Mirrors `ModelicaOp::AddVariable`. Variables and components share
/// the same `ClassDef.components: IndexMap` storage in rumoca's AST —
/// a "variable" is just a component with a non-empty
/// causality/variability prefix run. Renders the declaration via
/// `pretty::variable_decl` and lifts the parsed Component.
pub fn add_variable(
    class: &mut ClassDef,
    decl: &pretty::VariableDecl,
) -> Result<(), AstMutError> {
    if class.components.contains_key(&decl.name) {
        return Err(AstMutError::DuplicateComponent {
            class: class.name.text.to_string(),
            component: decl.name.clone(),
        });
    }
    let body = pretty::variable_decl(decl);
    let stub = format!("model __LunCoFragment\n{body}end __LunCoFragment;\n");
    let parsed = parse_to_ast(&stub, "__lunco_fragment.mo")
        .map_err(|_| AstMutError::ValueParseFailed { value: body.clone() })?;
    let parsed_class = parsed
        .classes
        .get("__LunCoFragment")
        .ok_or_else(|| AstMutError::ValueParseFailed { value: body.clone() })?;
    let new_component = parsed_class
        .components
        .get(&decl.name)
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: body })?;
    class.components.insert(decl.name.clone(), new_component);
    Ok(())
}

/// Remove a variable by name. Same storage as components — alias of
/// [`remove_component`] kept as its own entrypoint so `op_to_patch`
/// arms read 1:1 against the `ModelicaOp` variant they handle.
pub fn remove_variable(class: &mut ClassDef, name: &str) -> Result<(), AstMutError> {
    remove_component(class, name)
}

/// Add a new (empty) class definition inside `parent`.
///
/// Mirrors `ModelicaOp::AddClass`. `parent` is the dotted-qualified
/// path of the enclosing class — empty for top-level. Constructs an
/// empty `<kind> Name [partial] "description"` stub via
/// `pretty::class_block_empty`, parses, and inserts the parsed
/// ClassDef into the target's `classes` IndexMap (or
/// `StoredDefinition.classes` when `parent` is empty).
pub fn add_class(
    sd: &mut StoredDefinition,
    parent: &str,
    name: &str,
    kind: pretty::ClassKindSpec,
    description: &str,
    partial: bool,
) -> Result<(), AstMutError> {
    let stub_text = pretty::class_block_empty(name, kind, description, partial);
    let parsed = parse_to_ast(&stub_text, "__lunco_fragment.mo")
        .map_err(|_| AstMutError::ValueParseFailed { value: stub_text.clone() })?;
    let new_class = parsed
        .classes
        .get(name)
        .cloned()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: stub_text.clone() })?;
    if parent.is_empty() {
        if sd.classes.contains_key(name) {
            return Err(AstMutError::DuplicateClass {
                parent: String::from("(top-level)"),
                name: name.to_string(),
            });
        }
        sd.classes.insert(name.to_string(), new_class);
    } else {
        let parent_class = lookup_class_mut(sd, parent)?;
        if parent_class.classes.contains_key(name) {
            return Err(AstMutError::DuplicateClass {
                parent: parent.to_string(),
                name: name.to_string(),
            });
        }
        parent_class.classes.insert(name.to_string(), new_class);
    }
    Ok(())
}

/// Remove a class by qualified path. The last segment names the class
/// itself; the prefix names its enclosing scope.
///
/// Mirrors `ModelicaOp::RemoveClass`. Returns `ClassNotFound` when the
/// path is empty, the parent doesn't exist, or the leaf is missing.
pub fn remove_class(sd: &mut StoredDefinition, qualified: &str) -> Result<(), AstMutError> {
    if qualified.is_empty() {
        return Err(AstMutError::ClassNotFound(qualified.to_string()));
    }
    if let Some((parent, leaf)) = qualified.rsplit_once('.') {
        let parent_class = lookup_class_mut(sd, parent)?;
        if parent_class.classes.shift_remove(leaf).is_none() {
            return Err(AstMutError::ClassNotFound(qualified.to_string()));
        }
    } else if sd.classes.shift_remove(qualified).is_none() {
        return Err(AstMutError::ClassNotFound(qualified.to_string()));
    }
    Ok(())
}

/// Set or replace the `Placement(...)` annotation on a component.
///
/// Mirrors `ModelicaOp::SetPlacement`: locates `component` inside
/// `class`, finds any existing `Placement(...)` entry in
/// `Component.annotation` and replaces it; appends a fresh one if
/// absent. Other annotation entries (`Dialog`, `__LunCo`, …) are
/// preserved.
///
/// **How the new `Expression` is built:** rumoca's parser only consumes
/// whole files, so we wrap the rendered `Placement(...)` text in a
/// stub class and extract `comp.annotation[0]`. Same trick as
/// [`set_parameter`]'s `parse_value_fragment`. The placement payload
/// always parses cleanly because `pretty::placement_inner` produces
/// canonical text from typed numeric fields — no user-supplied
/// strings cross this boundary.
pub fn set_placement(
    class: &mut ClassDef,
    component: &str,
    placement: &pretty::Placement,
) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let comp = class
        .components
        .get_mut(component)
        .ok_or_else(|| AstMutError::ComponentNotFound {
            class: class_name,
            component: component.to_string(),
        })?;
    let new_placement_expr = parse_placement_expression(placement)?;
    if let Some(slot) = comp
        .annotation
        .iter_mut()
        .find(|expr| is_annotation_entry_named(expr, "Placement"))
    {
        *slot = new_placement_expr;
    } else {
        comp.annotation.push(new_placement_expr);
    }
    Ok(())
}

/// Parse a `Placement(...)` fragment into an [`Expression`] using the
/// stub-class trick (see [`parse_value_fragment`] for the rationale).
/// Returns the single annotation expression rumoca lifts onto the
/// component's `annotation` vector.
fn parse_placement_expression(placement: &pretty::Placement) -> Result<Expression, AstMutError> {
    let placement_text = pretty::placement_inner(placement);
    let stub = format!(
        "model __LunCoFragment\n  Real __v annotation({placement_text});\nend __LunCoFragment;\n"
    );
    let parsed = parse_to_ast(&stub, "__lunco_fragment.mo").map_err(|_| {
        AstMutError::ValueParseFailed { value: placement_text.clone() }
    })?;
    let class = parsed.classes.get("__LunCoFragment").ok_or_else(|| {
        AstMutError::ValueParseFailed { value: placement_text.clone() }
    })?;
    let comp = class.components.get("__v").ok_or_else(|| {
        AstMutError::ValueParseFailed { value: placement_text.clone() }
    })?;
    comp.annotation
        .first()
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: placement_text })
}

/// True when `expr` is `Name(...)` at the top level — used to find a
/// specific annotation entry (`Placement`, `Dialog`, `Icon`, …)
/// without descending into argument expressions.
///
/// Rumoca parses annotation entries as `Expression::ClassModification`
/// (the `Foo(x = 1, y = 2)` shape used for declaration / extends
/// modifications), *not* as `FunctionCall`. The two are syntactically
/// identical but semantically distinct (see the `Expression` enum
/// docstring on those variants). Annotation predicates must match
/// `ClassModification`.
fn is_annotation_entry_named(expr: &Expression, name: &str) -> bool {
    if let Expression::ClassModification { target, .. } = expr {
        target.parts.len() == 1 && &*target.parts[0].ident.text == name
    } else {
        false
    }
}

/// Parse a Modelica value fragment (the right-hand side of a binding
/// or modification: `"1.5"`, `"true"`, `"{1, 2}"`, etc.) into an
/// [`Expression`].
///
/// Rumoca's exposed parser entry point is whole-file — there's no
/// public expression-fragment entry. We work around it by wrapping the
/// fragment in a minimal stub class and extracting the parsed binding.
/// The stub-class identifier is fixed and namespace-private (`__LunCoFragment`)
/// so it can never collide with a real class in the user's code: it
/// only ever lives inside this throwaway parse.
fn parse_value_fragment(value_text: &str) -> Result<Expression, AstMutError> {
    let stub = format!("model __LunCoFragment\n  Real __v = {value_text};\nend __LunCoFragment;\n");
    let parsed = parse_to_ast(&stub, "__lunco_fragment.mo")
        .map_err(|_| AstMutError::ValueParseFailed { value: value_text.to_string() })?;
    let class = parsed
        .classes
        .get("__LunCoFragment")
        .ok_or_else(|| AstMutError::ValueParseFailed { value: value_text.to_string() })?;
    let comp = class
        .components
        .get("__v")
        .ok_or_else(|| AstMutError::ValueParseFailed { value: value_text.to_string() })?;
    comp.binding
        .as_ref()
        .cloned()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: value_text.to_string() })
}

/// Run an AST mutation against a class and return a `(byte_range,
/// replacement)` patch suitable for `Document::apply_patch`.
///
/// This is the seam where the AST-canonical migration lands. Today's
/// `op_to_patch` builds patches via `pretty::*` text emitters and
/// byte-range scans (`compute_set_placement_patch` etc.); after the
/// migration each AST-shaped op routes here:
///
/// 1. Resolve `class` to a [`ClassDef`] inside `parsed`.
/// 2. Clone it (we never mutate the input AST — the document's
///    snapshot stays valid for parallel readers, and rollback on
///    error is a no-op).
/// 3. Run `mutate(&mut clone)`.
/// 4. Detect the class's leading-whitespace indent from `source`.
/// 5. Emit the mutated class via `clone.to_modelica(indent)`.
/// 6. Return `(class.location range, regen_text)`.
///
/// The replaced span is the entire class. Undo grain is per-class
/// rather than per-modification — coarser than the legacy `pretty/`
/// path's surgical splices, but safe and trivial to verify against
/// the round-trip suite. If finer grain matters in practice we'll
/// diff `class.to_modelica()` before/after the mutation and splice
/// only the changed region; that's a follow-on optimisation.
///
/// `parse_error` is converted into [`AstMutError::ClassNotFound`]
/// when the class itself can't be resolved; mutation errors propagate
/// as-is.
pub fn regenerate_class_patch<F>(
    source: &str,
    parsed: &StoredDefinition,
    class: &str,
    mutate: F,
) -> Result<(Range<usize>, String), AstMutError>
where
    F: FnOnce(&mut ClassDef) -> Result<(), AstMutError>,
{
    // Clone the whole StoredDefinition so we can take a `&mut`
    // through `lookup_class_mut` without aliasing the caller's
    // snapshot. Cheap on lunco-sized models; cost scales with class
    // count, not modification depth.
    let mut sd_clone = parsed.clone();
    let class_def = lookup_class_mut(&mut sd_clone, class)?;
    mutate(class_def)?;

    // `class.location` in rumoca is class-name-to-`end-Name` — it
    // does NOT cover the leading kind keyword (`model`, `package`, …)
    // or any prefix modifiers (`partial`, `encapsulated`, …) that
    // appear in source, and it stops *before* the trailing `;` of
    // `end Name;`. `to_modelica` emits the full thing including
    // prefixes and trailing `end Name;\n`. We must therefore widen
    // both ends of the replaced span before splicing or we get
    // `model model M …;;` (prefix + name kept, body replaced, `;`
    // duplicated).
    let raw_start = class_def.location.start as usize;
    let raw_end = class_def.location.end as usize;
    let start = rewind_to_class_header_start(source, raw_start);
    let end = advance_past_trailing_semicolon(source, raw_end);

    // Indent inference: walk back from the *header* start to the line
    // start, capturing whitespace bytes only. Top-level classes
    // return ""; nested classes return the parent's inner-indent.
    let indent = leading_indent(source, start);
    let mut regen = class_def.to_modelica(&indent);
    // The ClassDef emitter starts with the indent prefix; the source
    // span we replace begins at the header start (after the indent
    // run on its line), so strip the indent from regen to keep the
    // surrounding bytes in `source` byte-stable.
    if regen.starts_with(&indent) {
        regen.drain(..indent.len());
    }
    // `to_modelica` ends with `\n`. Match the source span: if our
    // span ends at a `\n` in source, keep it; otherwise drop the
    // trailing `\n` from regen so we don't introduce an extra blank
    // line.
    if !ends_with_newline(source, end) && regen.ends_with('\n') {
        regen.pop();
    }

    Ok((start..end, regen))
}

/// Run an AST mutation against the whole `StoredDefinition` and
/// return a `(0..source.len(), regen)` whole-document patch.
///
/// Used by ops that change the document's class set: `AddClass` /
/// `RemoveClass`, where there's no single class span to splice
/// against. Whole-document replacement loses byte-stability for
/// unchanged classes (formatter may normalise whitespace), which is
/// the same trade-off the AST-canonical roadmap accepts on save.
pub fn regenerate_document_patch<F>(
    source: &str,
    parsed: &StoredDefinition,
    mutate: F,
) -> Result<(Range<usize>, String), AstMutError>
where
    F: FnOnce(&mut StoredDefinition) -> Result<(), AstMutError>,
{
    let mut sd_clone = parsed.clone();
    mutate(&mut sd_clone)?;
    let regen = sd_clone.to_modelica();
    Ok((0..source.len(), regen))
}

/// Walk back from `name_start` (the byte offset of the class name in
/// source) through any prefix-keyword run — `model`, `partial model`,
/// `encapsulated partial model`, `replaceable function`, … — up to the
/// first non-prefix character on the same logical line. Returns the
/// byte offset where the class declaration's first prefix keyword
/// begins.
///
/// Algorithm: skip whitespace (spaces/tabs only — newlines stop us),
/// then skip an ASCII-alphabetic word, then loop. Stop when we hit a
/// non-letter / non-space byte, a newline, or BOF. The position right
/// after the last skipped character is the header start.
fn rewind_to_class_header_start(source: &str, name_start: usize) -> usize {
    let bytes = source.as_bytes();
    if name_start > bytes.len() {
        return name_start;
    }
    let mut i = name_start;
    loop {
        // Trailing whitespace before the name (or between prefix words).
        while i > 0 {
            match bytes[i - 1] {
                b' ' | b'\t' => i -= 1,
                _ => break,
            }
        }
        // Word run (the keyword we're stepping over).
        let word_end = i;
        while i > 0 && bytes[i - 1].is_ascii_alphabetic() {
            i -= 1;
        }
        // No word stepped → we hit a non-word byte or BOF; we're done.
        if i == word_end {
            break;
        }
    }
    i
}

/// Advance past `end Name`'s trailing `;` (and an optional newline).
/// The AST `location.end` lands right after the `Name` token of the
/// `end Name` clause but before the semicolon. Matches the strategy
/// in rumoca's `full_span_with_leading_comments`.
fn advance_past_trailing_semicolon(source: &str, mut pos: usize) -> usize {
    let bytes = source.as_bytes();
    while pos < bytes.len() {
        match bytes[pos] {
            b' ' | b'\t' => pos += 1,
            b';' => {
                pos += 1;
                // Optionally swallow a single trailing newline so
                // regen's terminating `\n` lines up cleanly.
                if pos < bytes.len() && bytes[pos] == b'\n' {
                    pos += 1;
                }
                break;
            }
            _ => break,
        }
    }
    pos
}

/// Return the run of space/tab bytes immediately preceding `byte_pos`
/// up to (but not including) the previous newline. Used to recover the
/// indent string under which a class definition was originally written
/// so the regenerated class lines up with its source position.
fn leading_indent(source: &str, byte_pos: usize) -> String {
    if byte_pos > source.len() {
        return String::new();
    }
    let bytes = source.as_bytes();
    let mut start = byte_pos;
    while start > 0 {
        let c = bytes[start - 1];
        if c == b' ' || c == b'\t' {
            start -= 1;
        } else {
            break;
        }
    }
    // `start..byte_pos` is the indent run. Validate that what's
    // immediately before is a newline or BOF — otherwise the input
    // wasn't at line-start and we conservatively return "".
    if start == 0 || bytes[start - 1] == b'\n' {
        std::str::from_utf8(&bytes[start..byte_pos])
            .map(str::to_string)
            .unwrap_or_default()
    } else {
        String::new()
    }
}

fn ends_with_newline(source: &str, byte_end: usize) -> bool {
    byte_end > 0 && source.as_bytes().get(byte_end - 1) == Some(&b'\n')
}

/// Construct a synthetic [`Token`] for an identifier with no source
/// location. Useful when callers build AST fragments outside the parser
/// (e.g. modifications added by the canvas). Token-number / token-type
/// stay zero — name resolution and the typechecker re-run after every
/// mutation, so synthesised tokens get repopulated downstream.
#[allow(dead_code)] // wired in batch 2 (set_placement) — kept here so the
                   // construction policy lives next to the helpers that
                   // need it.
pub(crate) fn synth_token(text: impl Into<Arc<str>>) -> Token {
    Token {
        text: text.into(),
        location: Default::default(),
        token_number: 0,
        token_type: 0,
    }
}

/// Construct a synthetic terminal expression (numeric literal, string,
/// bool). Pairs with [`synth_token`] for the unusual case where parsing
/// a fragment is overkill — e.g. fixed boolean modifications from a
/// checkbox in the inspector.
#[allow(dead_code)]
pub(crate) fn synth_terminal(terminal_type: TerminalType, text: impl Into<Arc<str>>) -> Expression {
    Expression::Terminal {
        terminal_type,
        token: synth_token(text),
    }
}
