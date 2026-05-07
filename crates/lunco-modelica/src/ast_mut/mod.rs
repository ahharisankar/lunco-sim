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

use std::sync::Arc;

use rumoca_session::parsing::ast::{ClassDef, Expression, StoredDefinition, Token, TerminalType};
use rumoca_phase_parse::parse_to_ast;

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
