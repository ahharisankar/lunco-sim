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
//! Batch 1 of A.2: `set_parameter`. Smallest blast radius — modifies
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
    /// No `__LunCo_PlotNode(signal=…)` matched in the class's
    /// `Diagram(graphics)` array.
    PlotNodeNotFound {
        /// Class whose Diagram annotation was searched.
        class: String,
        /// Signal path that was not found.
        signal: String,
    },
    /// `set_diagram_text_*` / `remove_diagram_text` was given an index
    /// past the end of the Text-only sequence in `Diagram(graphics)`.
    DiagramTextIndexOutOfRange {
        /// Class whose Diagram annotation was searched.
        class: String,
        /// Index requested by the caller.
        index: usize,
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
            AstMutError::PlotNodeNotFound { class, signal } => write!(
                f,
                "no __LunCo_PlotNode with signal `{signal}` in class `{class}`"
            ),
            AstMutError::DiagramTextIndexOutOfRange { class, index } => write!(
                f,
                "Diagram text index {index} out of range in class `{class}`"
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
/// Parse a `__LunCoFragment` stub class and return the resulting
/// `StoredDefinition`, **memoised by stub text**.
///
/// Every fragment-parse helper below (`parse_value_fragment`,
/// `parse_placement_expression`, `parse_component_fragment`, …) wraps
/// its input in a stub class and parses the whole thing. Rumoca's
/// public parser entry is whole-file only; there's no public
/// expression-fragment entry, so the stub-class trick is the only
/// portable way to extract a parsed `Expression` / `Component` /
/// `Equation`.
///
/// The same stub text recurs constantly in normal use:
///   - drag-bursts emit identical `Placement(...)` strings as the
///     mouse hovers between integer pixel positions,
///   - palette-drop / AddComponent reuses the same defaults skeleton,
///   - parameter sliders re-emit identical numeric literals,
///   - typical scenes have many components with the same `Placement`
///     extent (only `origin` varies).
///
/// Roadmap step 5 of the AST-canonical refactor: don't re-parse the
/// same stub text on every op. A bounded process-wide cache (capped
/// at 1024 entries; cleared wholesale when full — eviction policy
/// doesn't matter much because most hits are within drag bursts on
/// the same handful of strings) keeps each parse to a hash lookup +
/// `Arc::clone` after first sight. First-time parses still pay the
/// full rumoca cost.
///
/// Returns `None` if rumoca couldn't parse the stub — callers map this
/// onto their domain-specific [`AstMutError`] variant. The cache stores
/// only successes.
fn parse_stub_cached(stub: &str) -> Option<std::sync::Arc<StoredDefinition>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, std::sync::Arc<StoredDefinition>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::with_capacity(64)));

    if let Some(hit) = cache.lock().unwrap().get(stub).cloned() {
        return Some(hit);
    }

    let parsed = parse_to_ast(stub, "__lunco_fragment.mo").ok()?;
    let arc = std::sync::Arc::new(parsed);

    let mut g = cache.lock().unwrap();
    // Bounded cap. We don't need true LRU — fragment populations are
    // small (~hundreds of unique placements in a real session), and
    // wholesale clear-on-overflow is cheaper than tracking ages.
    // Tune the cap if real workloads ever push past it.
    if g.len() >= 1024 {
        g.clear();
    }
    g.insert(stub.to_string(), arc.clone());
    Some(arc)
}

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
/// `param == ""` is the sentinel for the component's *primary
/// binding* — the `= expr` after the name, used to mutate top-level
/// parameter declarations like `parameter Real k = 5;`. `param ==
/// "start"` routes to the dedicated `start` field. Anything else
/// goes through the generic `modifications` map. Add a row when a
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
        "" => {
            // Primary binding (`parameter Real g = 9.81`). Rumoca's
            // `Component::to_modelica` formatter (rumoca-ir-ast/
            // src/modelica.rs around line 294) emits `= {self.start}`
            // when `has_explicit_binding` is true — it reads from
            // `start`, NOT from `binding`. The `binding` field exists
            // for query consumers but is ignored on round-trip. Write
            // both fields so any consumer reads the same value, but
            // `start` + `has_explicit_binding` is what re-emits.
            // Clear `start_is_modification` so the formatter doesn't
            // ALSO emit `(start = …)` alongside the `= …` binding.
            comp.binding = Some(expr.clone());
            comp.start = expr;
            comp.has_explicit_binding = true;
            comp.start_is_modification = false;
        }
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
/// `set_parameter`'s `parse_value_fragment`. Errors out if a
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

/// Set or clear the `annotation(Line(points={...}))` on a
/// `connect(...)` equation matching `(from, to)`. Empty `points` clears
/// the annotation entirely (wire falls back to auto-routing on next
/// projection). Returns `ConnectionNotFound` when no match exists.
///
/// Implemented by rendering a stub `connect(...) annotation(Line(...))`
/// via `pretty::connect_equation`, parsing it, and stealing the parsed
/// `annotation: Vec<Expression>` to overwrite the target equation's
/// annotation field. Keeps annotation-expression construction in one
/// place (the parser) instead of building Expression nodes by hand.
pub fn set_connection_line(
    class: &mut ClassDef,
    from: &pretty::PortRef,
    to: &pretty::PortRef,
    points: &[(f32, f32)],
) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let new_annotation: Vec<rumoca_session::parsing::ast::Expression> = if points.is_empty() {
        Vec::new()
    } else {
        let stub_eq = pretty::ConnectEquation {
            from: from.clone(),
            to: to.clone(),
            line: Some(pretty::Line { points: points.to_vec() }),
        };
        let parsed = parse_connect_equation_with_annotation_fragment(&stub_eq)?;
        match parsed {
            rumoca_session::parsing::ast::Equation::Connect { annotation, .. } => annotation,
            _ => return Err(AstMutError::ValueParseFailed { value: "connect annotation".into() }),
        }
    };
    let mut matched = false;
    for eq in class.equations.iter_mut() {
        if let rumoca_session::parsing::ast::Equation::Connect { lhs, rhs, annotation } = eq {
            if matches_port_ref(lhs, from) && matches_port_ref(rhs, to) {
                *annotation = new_annotation;
                matched = true;
                break;
            }
        }
    }
    if !matched {
        return Err(AstMutError::ConnectionNotFound {
            class: class_name,
            from: format!("{}.{}", from.component, from.port),
            to: format!("{}.{}", to.component, to.port),
        });
    }
    Ok(())
}

/// Variant of [`parse_connect_equation_fragment`] that renders via
/// `pretty::connect_equation` so the `annotation(Line(...))` is
/// included. Used by `set_connection_line` to obtain a parsed
/// annotation tree shaped exactly like the source emitter would write.
fn parse_connect_equation_with_annotation_fragment(
    eq: &pretty::ConnectEquation,
) -> Result<rumoca_session::parsing::ast::Equation, AstMutError> {
    let body = pretty::connect_equation(eq);
    let stub = format!("model __LunCoFragment\nequation\n{body}end __LunCoFragment;\n");
    let parsed = parse_stub_cached(&stub).ok_or_else(|| {
        AstMutError::ValueParseFailed { value: body.clone() }
    })?;
    let cls = parsed.classes.get("__LunCoFragment").ok_or_else(|| {
        AstMutError::ValueParseFailed { value: body.clone() }
    })?;
    cls.equations
        .first()
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: body })
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
    let parsed = parse_stub_cached(&stub).ok_or_else(|| {
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
/// is deleted this becomes the only emitter for connect
/// equations.
fn parse_connect_equation_fragment(
    eq: &pretty::ConnectEquation,
) -> Result<rumoca_session::parsing::ast::Equation, AstMutError> {
    let from_text = render_port_ref(&eq.from);
    let to_text = render_port_ref(&eq.to);
    let body = format!("  connect({from_text}, {to_text});\n");
    let stub = format!("model __LunCoFragment\nequation\n{body}end __LunCoFragment;\n");
    let parsed = parse_stub_cached(&stub).ok_or_else(|| {
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
    let parsed = parse_stub_cached(&stub)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: body.clone() })?;
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
    let parsed = parse_stub_cached(&stub_text)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: stub_text.clone() })?;
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

/// Add a short-class definition (`connector X = Y(...)`) inside
/// `parent` (or at top level when `parent` is empty).
///
/// Mirrors `ModelicaOp::AddShortClass`. Same insert path as
/// [`add_class`]; differs only in how the new ClassDef is built —
/// `pretty::short_class_decl` emits the `kind Name = base(...)` form
/// which the parser lifts into a `ClassDef` with a `class_type` of
/// the short flavour.
pub fn add_short_class(
    sd: &mut StoredDefinition,
    parent: &str,
    name: &str,
    kind: pretty::ClassKindSpec,
    base: &str,
    prefixes: &[String],
    modifications: &[(String, String)],
) -> Result<(), AstMutError> {
    let stub_text = pretty::short_class_decl(name, kind, base, prefixes, modifications);
    let parsed = parse_stub_cached(&stub_text)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: stub_text.clone() })?;
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

/// Append a generic equation to a class.
///
/// Mirrors `ModelicaOp::AddEquation`. Renders the equation via
/// `pretty::equation_decl`, wraps in a stub class equation section,
/// parses, and pushes the lifted Equation onto the target's
/// `equations` list. Unlike [`add_connection`] (which always emits
/// `Equation::Connect`), this accepts any equation shape — `a = b`,
/// `assert(...)`, `der(x) = …` — whatever `pretty::equation_decl`
/// produces.
pub fn add_equation(
    class: &mut ClassDef,
    eq: &pretty::EquationDecl,
) -> Result<(), AstMutError> {
    let body = pretty::equation_decl(eq);
    let stub = format!("model __LunCoFragment\nequation\n{body}end __LunCoFragment;\n");
    let parsed = parse_stub_cached(&stub)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: body.clone() })?;
    let parsed_class = parsed
        .classes
        .get("__LunCoFragment")
        .ok_or_else(|| AstMutError::ValueParseFailed { value: body.clone() })?;
    let new_eq = parsed_class
        .equations
        .first()
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: body })?;
    class.equations.push(new_eq);
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

// ─────────────────────────────────────────────────────────────────────────
// Annotation graphics-tree helpers (A.2 batch 3b — graphics ops)
// ─────────────────────────────────────────────────────────────────────────
//
// Plot / Diagram-text / Icon-graphic ops navigate the class annotation
// tree:
//
//   class.annotation: Vec<Expression>
//     └── Diagram(graphics={...})           ← Expression::ClassModification
//          └── graphics                     ← Expression::Modification with
//               └── Array { elements }           value = Expression::Array
//                    ├── __LunCo_PlotNode(signal=..., extent=..., title=...)
//                    ├── Text(extent=..., textString=...)
//                    └── Line(...)/Rectangle(...)/...
//
// `Icon` mirrors `Diagram`. The shared work is "find or create a named
// section, find or create its `graphics` array, return `&mut Vec<Expression>`."
// Per-op work then does push / retain / find-and-update on that array.

/// Get a mutable reference to the `graphics={...}` array inside the
/// class's `Diagram` or `Icon` annotation section, creating the
/// section and the `graphics={}` array if either is missing.
///
/// `section_name` is `"Diagram"` or `"Icon"`. Other names are accepted
/// — the helper doesn't gatekeep — but the only useful targets in
/// practice are those two.
fn graphics_array_mut<'a>(
    class: &'a mut ClassDef,
    section_name: &str,
) -> &'a mut Vec<Expression> {
    // Step 1: locate or create the `<section>(...)` ClassModification.
    let section_idx = class
        .annotation
        .iter()
        .position(|e| is_annotation_entry_named(e, section_name));
    let section_idx = match section_idx {
        Some(i) => i,
        None => {
            // Insert an empty section: `<Section>()`.
            class.annotation.push(Expression::ClassModification {
                target: rumoca_session::parsing::ast::ComponentReference {
                    local: false,
                    parts: vec![rumoca_session::parsing::ast::ComponentRefPart {
                        ident: synth_token(section_name.to_string()),
                        subs: None,
                    }],
                    def_id: None,
                },
                modifications: Vec::new(),
            });
            class.annotation.len() - 1
        }
    };
    let mods = match &mut class.annotation[section_idx] {
        Expression::ClassModification { modifications, .. } => modifications,
        // Unreachable: we just selected by predicate / inserted as
        // ClassModification. Asserting via `unreachable!` keeps the
        // type system honest without an `Option` we'd never observe.
        _ => unreachable!("section was a ClassModification on insert/find"),
    };

    // Step 2: locate or create the `graphics = {...}` Modification.
    let graphics_idx = mods.iter().position(|m| {
        matches!(
            m,
            Expression::Modification { target, .. }
                if target.parts.len() == 1
                    && &*target.parts[0].ident.text == "graphics"
        )
    });
    let graphics_idx = match graphics_idx {
        Some(i) => i,
        None => {
            mods.push(Expression::Modification {
                target: rumoca_session::parsing::ast::ComponentReference {
                    local: false,
                    parts: vec![rumoca_session::parsing::ast::ComponentRefPart {
                        ident: synth_token("graphics".to_string()),
                        subs: None,
                    }],
                    def_id: None,
                },
                value: Arc::new(Expression::Array {
                    elements: Vec::new(),
                    is_matrix: false,
                }),
            });
            mods.len() - 1
        }
    };
    let graphics_value = match &mut mods[graphics_idx] {
        Expression::Modification { value, .. } => Arc::make_mut(value),
        _ => unreachable!("graphics modification just inserted/found above"),
    };
    match graphics_value {
        Expression::Array { elements, .. } => elements,
        // The graphics modification may have been parsed with a
        // non-array value in pathological inputs (`graphics = 1`); in
        // that case overwrite with an empty array. Keeps callers from
        // having to handle a third branch.
        other => {
            *other = Expression::Array {
                elements: Vec::new(),
                is_matrix: false,
            };
            match other {
                Expression::Array { elements, .. } => elements,
                _ => unreachable!("just assigned an Array variant"),
            }
        }
    }
}

/// Append a graphic to `class.annotation.<section>(graphics)`.
///
/// `graphic_text` is the rendered fragment (`Rectangle(...)`,
/// `Line(...)`, etc.) — built by the caller via the matching
/// `pretty::*` helper. Lifted via `parse_graphics_entry` so the
/// inserted Expression takes the `FunctionCall` shape the parser
/// uses for inside-array context.
fn append_graphic_to_section(
    class: &mut ClassDef,
    section_name: &str,
    graphic_text: &str,
) -> Result<(), AstMutError> {
    let entry = parse_graphics_entry(graphic_text)?;
    let arr = graphics_array_mut(class, section_name);
    arr.push(entry);
    Ok(())
}

/// Parse a fragment destined for a graphics array (`{Foo(...), Bar(...)}`)
/// by wrapping it inside a `Diagram(graphics={text})` annotation and
/// lifting the array's first element. Returns an
/// `Expression::FunctionCall` — the variant the parser uses for
/// inside-array entries (vs `ClassModification` at top level).
fn parse_graphics_entry(text: &str) -> Result<Expression, AstMutError> {
    let stub = format!(
        "model __LunCoFragment\nannotation(Diagram(graphics={{{text}}}));\nend __LunCoFragment;\n"
    );
    let parsed = parse_stub_cached(&stub)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let class = parsed
        .classes
        .get("__LunCoFragment")
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let diagram = class
        .annotation
        .first()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let Expression::ClassModification { modifications, .. } = diagram else {
        return Err(AstMutError::ValueParseFailed { value: text.to_string() });
    };
    let graphics_mod = modifications
        .iter()
        .find_map(|m| match m {
            Expression::Modification { target, value }
                if target.parts.len() == 1
                    && &*target.parts[0].ident.text == "graphics" =>
            {
                Some(value)
            }
            _ => None,
        })
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let Expression::Array { elements, .. } = graphics_mod.as_ref() else {
        return Err(AstMutError::ValueParseFailed { value: text.to_string() });
    };
    elements
        .first()
        .cloned()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })
}

/// True when `expr` is a graphics-array entry whose head identifier
/// matches `name`. Handles both shapes the parser produces depending
/// on context: `FunctionCall { comp }` (inside an array) and
/// `ClassModification { target }` (top-level).
fn is_graphic_entry_named(expr: &Expression, name: &str) -> bool {
    match expr {
        Expression::FunctionCall { comp, .. } => {
            comp.parts.len() == 1 && &*comp.parts[0].ident.text == name
        }
        Expression::ClassModification { target, .. } => {
            target.parts.len() == 1 && &*target.parts[0].ident.text == name
        }
        _ => false,
    }
}

/// Look up a named argument / modification by key inside a
/// graphics-array entry. Returns the value Expression. Handles both
/// `FunctionCall { args: NamedArgument }` and
/// `ClassModification { modifications: Modification }` shapes.
fn graphic_entry_arg<'a>(expr: &'a Expression, key: &str) -> Option<&'a Expression> {
    match expr {
        Expression::FunctionCall { args, .. } => {
            for a in args {
                if let Expression::NamedArgument { name, value } = a {
                    if &*name.text == key {
                        return Some(value.as_ref());
                    }
                }
            }
            None
        }
        Expression::ClassModification { modifications, .. } => {
            for m in modifications {
                if let Expression::Modification { target, value } = m {
                    if target.parts.len() == 1
                        && &*target.parts[0].ident.text == key
                    {
                        return Some(value.as_ref());
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Add a `__LunCo_PlotNode(...)` entry to the class's `Diagram(graphics)`
/// array. Idempotency: if a plot node with the same `signal=` already
/// exists, it is replaced (not duplicated).
pub fn add_plot_node(
    class: &mut ClassDef,
    plot: &pretty::LunCoPlotNodeSpec,
) -> Result<(), AstMutError> {
    let new_entry = parse_graphics_entry(&pretty::lunco_plot_node_inner(plot))?;
    let arr = graphics_array_mut(class, "Diagram");
    let signal = plot.signal.clone();
    if let Some(slot) = arr
        .iter_mut()
        .find(|e| plot_node_signal_matches(e, &signal))
    {
        *slot = new_entry;
    } else {
        arr.push(new_entry);
    }
    Ok(())
}

/// Remove the `__LunCo_PlotNode(...)` entry whose `signal=` matches.
pub fn remove_plot_node(class: &mut ClassDef, signal_path: &str) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let arr = graphics_array_mut(class, "Diagram");
    let before = arr.len();
    arr.retain(|e| !plot_node_signal_matches(e, signal_path));
    if arr.len() == before {
        return Err(AstMutError::PlotNodeNotFound {
            class: class_name,
            signal: signal_path.to_string(),
        });
    }
    Ok(())
}

/// Update the `extent={{x1,y1},{x2,y2}}` of a plot node by signal.
pub fn set_plot_node_extent(
    class: &mut ClassDef,
    signal_path: &str,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
) -> Result<(), AstMutError> {
    update_plot_node_by_signal(class, signal_path, |spec| {
        spec.x1 = x1;
        spec.y1 = y1;
        spec.x2 = x2;
        spec.y2 = y2;
    })
}

/// Update the `title=` of a plot node by signal. Empty string removes
/// the title from the rendered form (per `lunco_plot_node_inner`).
pub fn set_plot_node_title(
    class: &mut ClassDef,
    signal_path: &str,
    title: &str,
) -> Result<(), AstMutError> {
    update_plot_node_by_signal(class, signal_path, |spec| {
        spec.title = title.to_string();
    })
}

/// Helper: read the existing `__LunCo_PlotNode` matching `signal_path`
/// back into a `LunCoPlotNodeSpec`, run `update`, and re-emit. The
/// re-emit-via-pretty path keeps the on-disk shape canonical without
/// us having to navigate `Modification` trees field-by-field. Matches
/// today's `compute_set_plot_node_*_patch` semantics.
fn update_plot_node_by_signal<F>(
    class: &mut ClassDef,
    signal_path: &str,
    update: F,
) -> Result<(), AstMutError>
where
    F: FnOnce(&mut pretty::LunCoPlotNodeSpec),
{
    let class_name = class.name.text.to_string();
    let arr = graphics_array_mut(class, "Diagram");
    let entry = arr
        .iter_mut()
        .find(|e| plot_node_signal_matches(e, signal_path))
        .ok_or_else(|| AstMutError::PlotNodeNotFound {
            class: class_name,
            signal: signal_path.to_string(),
        })?;
    let mut spec = read_plot_node_spec(entry);
    update(&mut spec);
    *entry = parse_graphics_entry(&pretty::lunco_plot_node_inner(&spec))?;
    Ok(())
}

/// Predicate: is `expr` a `__LunCo_PlotNode(...)` whose `signal=`
/// modification matches `target_signal`? Handles both the
/// `FunctionCall { args: NamedArgument }` shape used inside graphics
/// arrays and the `ClassModification { modifications: Modification }`
/// shape used at top level — the parser picks based on context.
fn plot_node_signal_matches(expr: &Expression, target_signal: &str) -> bool {
    if !is_graphic_entry_named(expr, "__LunCo_PlotNode") {
        return false;
    }
    matches!(
        graphic_entry_arg(expr, "signal"),
        Some(v) if string_literal_value(v) == Some(target_signal.to_string())
    )
}

/// Pull the signal/extent/title/etc. fields out of a parsed
/// `__LunCo_PlotNode(...)` Expression back into a `LunCoPlotNodeSpec`.
/// Default values for any field the Expression doesn't carry — the
/// canvas's emit path always writes signal+extent+title, so missing
/// fields only surface for hand-edited annotations.
fn read_plot_node_spec(expr: &Expression) -> pretty::LunCoPlotNodeSpec {
    let mut spec = pretty::LunCoPlotNodeSpec {
        x1: 0.0,
        y1: 0.0,
        x2: 0.0,
        y2: 0.0,
        signal: String::new(),
        title: String::new(),
    };
    if let Some(v) = graphic_entry_arg(expr, "signal") {
        if let Some(s) = string_literal_value(v) {
            spec.signal = s;
        }
    }
    if let Some(v) = graphic_entry_arg(expr, "title") {
        if let Some(s) = string_literal_value(v) {
            spec.title = s;
        }
    }
    if let Some(v) = graphic_entry_arg(expr, "extent") {
        // `extent = {{x1,y1},{x2,y2}}` → outer Array of two inner
        // Arrays of two numbers each. Malformed inputs fall through
        // and leave the spec at default — the canvas overwrites on
        // its next gesture anyway.
        if let Expression::Array { elements: outer, .. } = v {
            if outer.len() == 2 {
                if let (Some((x1, y1)), Some((x2, y2))) =
                    (point_pair(&outer[0]), point_pair(&outer[1]))
                {
                    spec.x1 = x1;
                    spec.y1 = y1;
                    spec.x2 = x2;
                    spec.y2 = y2;
                }
            }
        }
    }
    spec
}

fn point_pair(e: &Expression) -> Option<(f32, f32)> {
    if let Expression::Array { elements, .. } = e {
        if elements.len() == 2 {
            let x = number_literal_value(&elements[0])?;
            let y = number_literal_value(&elements[1])?;
            return Some((x as f32, y as f32));
        }
    }
    None
}

fn number_literal_value(e: &Expression) -> Option<f64> {
    match e {
        Expression::Terminal { token, .. } => token.text.parse::<f64>().ok(),
        Expression::Unary { op, rhs }
            if matches!(op, rumoca_session::parsing::ast::OpUnary::Minus(_)) =>
        {
            number_literal_value(rhs).map(|v| -v)
        }
        _ => None,
    }
}

fn string_literal_value(e: &Expression) -> Option<String> {
    let Expression::Terminal { terminal_type, token } = e else { return None };
    if !matches!(terminal_type, TerminalType::String) {
        return None;
    }
    // The lexer keeps quotes on string literals — strip them here so
    // the comparison with our `target_signal` works on the inner
    // text only.
    let raw: &str = &token.text;
    let trimmed = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(raw);
    Some(
        trimmed
            .replace("\\\"", "\"")
            .replace("\\\\", "\\"),
    )
}

/// Set or replace the `extent` of the i-th `Text(...)` entry in
/// `Diagram(graphics)`. Index counts Text entries only, in source
/// order — matches `ModelicaOp::SetDiagramTextExtent` and the legacy
/// `compute_set_diagram_text_extent_patch`.
pub fn set_diagram_text_extent(
    class: &mut ClassDef,
    index: usize,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
) -> Result<(), AstMutError> {
    update_diagram_text_at(class, index, |spec| {
        spec.x1 = x1;
        spec.y1 = y1;
        spec.x2 = x2;
        spec.y2 = y2;
    })
}

/// Set or replace the `textString=` of the i-th `Text(...)` entry.
pub fn set_diagram_text_string(
    class: &mut ClassDef,
    index: usize,
    text: &str,
) -> Result<(), AstMutError> {
    update_diagram_text_at(class, index, |spec| {
        spec.text = text.to_string();
    })
}

/// Remove the i-th `Text(...)` entry from `Diagram(graphics)`.
pub fn remove_diagram_text(class: &mut ClassDef, index: usize) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let arr = graphics_array_mut(class, "Diagram");
    let mut text_seen = 0usize;
    let mut target_idx = None;
    for (i, e) in arr.iter().enumerate() {
        if is_graphic_entry_named(e, "Text") {
            if text_seen == index {
                target_idx = Some(i);
                break;
            }
            text_seen += 1;
        }
    }
    let i = target_idx.ok_or(AstMutError::DiagramTextIndexOutOfRange {
        class: class_name,
        index,
    })?;
    arr.remove(i);
    Ok(())
}

/// Read i-th Text entry into a `pretty::GraphicSpec::Text` (defaulting
/// missing fields), call `update`, re-emit, replace.
fn update_diagram_text_at<F>(
    class: &mut ClassDef,
    index: usize,
    update: F,
) -> Result<(), AstMutError>
where
    F: FnOnce(&mut TextSpec),
{
    let class_name = class.name.text.to_string();
    let arr = graphics_array_mut(class, "Diagram");
    let mut text_seen = 0usize;
    let mut target_idx = None;
    for (i, e) in arr.iter().enumerate() {
        if is_graphic_entry_named(e, "Text") {
            if text_seen == index {
                target_idx = Some(i);
                break;
            }
            text_seen += 1;
        }
    }
    let i = target_idx.ok_or(AstMutError::DiagramTextIndexOutOfRange {
        class: class_name,
        index,
    })?;
    let mut spec = read_text_spec(&arr[i]);
    update(&mut spec);
    arr[i] = parse_graphics_entry(&render_text_spec(&spec))?;
    Ok(())
}

/// A trimmed `Text(...)` graphic — only the fields any of the three
/// canvas-driven Text ops ever read or write. Avoids round-tripping
/// the full `GraphicSpec::Text` (which has color / font fields the
/// canvas ops don't touch).
struct TextSpec {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    text: String,
}

fn read_text_spec(expr: &Expression) -> TextSpec {
    let mut spec = TextSpec {
        x1: 0.0,
        y1: 0.0,
        x2: 0.0,
        y2: 0.0,
        text: String::new(),
    };
    if let Some(v) = graphic_entry_arg(expr, "extent") {
        if let Expression::Array { elements: outer, .. } = v {
            if outer.len() == 2 {
                if let (Some((x1, y1)), Some((x2, y2))) =
                    (point_pair(&outer[0]), point_pair(&outer[1]))
                {
                    spec.x1 = x1;
                    spec.y1 = y1;
                    spec.x2 = x2;
                    spec.y2 = y2;
                }
            }
        }
    }
    if let Some(v) = graphic_entry_arg(expr, "textString") {
        if let Some(s) = string_literal_value(v) {
            spec.text = s;
        }
    }
    spec
}

fn render_text_spec(spec: &TextSpec) -> String {
    // Canonical form: `Text(extent={{x1,y1},{x2,y2}}, textString="...")`.
    // Keeps the emit path narrow — color/font preservation lives in
    // the legacy `compute_set_diagram_text_*_patch` helpers, which
    // re-render the full GraphicSpec including the original colors.
    // After A.4 we replace those by extending TextSpec to carry every
    // field; for canvas-driven flows the trimmed form is sufficient.
    let escaped = spec.text.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "Text(extent={{{{{},{}}},{{{},{}}}}}, textString=\"{}\")",
        spec.x1, spec.y1, spec.x2, spec.y2, escaped
    )
}

/// Append a graphic to `Icon(graphics)` or `Diagram(graphics)`.
///
/// `graphic_text` is built by the caller — typically via
/// `pretty::graphic_inner` — so this helper stays oblivious to the
/// `GraphicSpec` payload shape.
pub fn add_named_graphic(
    class: &mut ClassDef,
    section_name: &str,
    graphic_text: &str,
) -> Result<(), AstMutError> {
    append_graphic_to_section(class, section_name, graphic_text)
}

/// Set or replace the class-level `experiment(...)` annotation.
///
/// Mirrors `ModelicaOp::SetExperimentAnnotation`. The class
/// annotation list (`ClassDef.annotation: Vec<Expression>`) holds
/// flat top-level entries — `Diagram(...)`, `Icon(...)`,
/// `experiment(...)`, vendor entries — and `experiment` always lives
/// at this level (never nested inside `Diagram` or `Icon`). Find the
/// existing entry; replace. If absent, append.
pub fn set_experiment(
    class: &mut ClassDef,
    start_time: f64,
    stop_time: f64,
    tolerance: f64,
    interval: f64,
) -> Result<(), AstMutError> {
    let new_expr = parse_experiment_expression(start_time, stop_time, tolerance, interval)?;
    if let Some(slot) = class
        .annotation
        .iter_mut()
        .find(|expr| is_annotation_entry_named(expr, "experiment"))
    {
        *slot = new_expr;
    } else {
        class.annotation.push(new_expr);
    }
    Ok(())
}

/// Build the `experiment(StartTime=…, StopTime=…, Tolerance=…,
/// Interval=…)` Expression via the stub-class trick. Numbers are
/// rendered through `pretty::experiment_inner`, which produces a
/// canonical fragment that always parses cleanly — no user-supplied
/// strings cross the boundary, so the parse can't fail except in the
/// pathological case of a NaN/Infinity input (which `pretty/`
/// already screens for upstream).
fn parse_experiment_expression(
    start_time: f64,
    stop_time: f64,
    tolerance: f64,
    interval: f64,
) -> Result<Expression, AstMutError> {
    let inner = pretty::experiment_inner(start_time, stop_time, tolerance, interval);
    let stub = format!("model __LunCoFragment\nannotation({inner});\nend __LunCoFragment;\n");
    let parsed = parse_stub_cached(&stub).ok_or_else(|| {
        AstMutError::ValueParseFailed { value: inner.clone() }
    })?;
    let class = parsed.classes.get("__LunCoFragment").ok_or_else(|| {
        AstMutError::ValueParseFailed { value: inner.clone() }
    })?;
    class
        .annotation
        .first()
        .cloned()
        .ok_or(AstMutError::ValueParseFailed { value: inner })
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
/// `set_parameter`'s `parse_value_fragment`. The placement payload
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
    let parsed = parse_stub_cached(&stub).ok_or_else(|| {
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
    let parsed = parse_stub_cached(&stub)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: value_text.to_string() })?;
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
) -> Result<(Range<usize>, String, std::sync::Arc<StoredDefinition>), AstMutError>
where
    F: FnOnce(&mut ClassDef) -> Result<(), AstMutError>,
{
    // Clone the whole StoredDefinition so we can take a `&mut`
    // through `lookup_class_mut` without aliasing the caller's
    // snapshot. Cheap on lunco-sized models; cost scales with class
    // count, not modification depth.
    //
    // The mutated clone is the third return value: the document layer
    // installs it directly into the SyntaxCache so consumers
    // (engine session, projection, sibling tabs) see the fresh AST
    // without rumoca re-parsing. Source-as-canonical re-parse round-trip
    // was a 1-8s window per drag — eliminated by handing back the AST
    // we just produced.
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

    Ok((start..end, regen, std::sync::Arc::new(sd_clone)))
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
) -> Result<(Range<usize>, String, std::sync::Arc<StoredDefinition>), AstMutError>
where
    F: FnOnce(&mut StoredDefinition) -> Result<(), AstMutError>,
{
    let mut sd_clone = parsed.clone();
    mutate(&mut sd_clone)?;
    let regen = sd_clone.to_modelica();
    Ok((0..source.len(), regen, std::sync::Arc::new(sd_clone)))
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
pub(crate) fn synth_token(text: impl Into<Arc<str>>) -> Token {
    Token {
        text: text.into(),
        location: Default::default(),
        token_number: 0,
        token_type: 0,
    }
}
