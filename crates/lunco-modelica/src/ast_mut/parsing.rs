//! Fragment parsing and stub-class trick helpers.

use std::sync::Arc;
use rumoca_session::parsing::ast::StoredDefinition;
use rumoca_phase_parse::parse_to_ast;
use super::errors::AstMutError;
use crate::pretty;

/// Parse a `__LunCoFragment` stub class and return the resulting
/// `StoredDefinition`, **memoised by stub text**.
pub(crate) fn parse_stub_cached(stub: &str) -> Option<Arc<StoredDefinition>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, Arc<StoredDefinition>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::with_capacity(64)));

    if let Some(hit) = cache.lock().unwrap().get(stub).cloned() {
        return Some(hit);
    }

    let parsed = parse_to_ast(stub, "__lunco_fragment.mo").ok()?;
    let arc = Arc::new(parsed);

    let mut g = cache.lock().unwrap();
    if g.len() >= 1024 {
        g.clear();
    }
    g.insert(stub.to_string(), arc.clone());
    Some(arc)
}

/// Parse a Modelica value fragment (the right-hand side of a binding
/// or modification: `"1.5"`, `"true"`, `"{1, 2}"`, etc.) into an
/// [`rumoca_session::parsing::ast::Expression`].
pub(crate) fn parse_value_fragment(value_text: &str) -> Result<rumoca_session::parsing::ast::Expression, AstMutError> {
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

/// Parse a `pretty::ComponentDecl` into a rumoca [`Component`] by
/// wrapping it in a stub class.
pub(crate) fn parse_component_fragment(
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
pub(crate) fn parse_connect_equation_fragment(
    eq: &pretty::ConnectEquation,
) -> Result<rumoca_session::parsing::ast::Equation, AstMutError> {
    let body = pretty::connect_equation(eq);
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

/// Parse a fragment destined for a graphics array (`{Foo(...), Bar(...)}`).
pub(crate) fn parse_graphics_entry(text: &str) -> Result<rumoca_session::parsing::ast::Expression, AstMutError> {
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
    let rumoca_session::parsing::ast::Expression::ClassModification { modifications, .. } = diagram else {
        return Err(AstMutError::ValueParseFailed { value: text.to_string() });
    };
    let graphics_mod = modifications
        .iter()
        .find_map(|m| match m {
            rumoca_session::parsing::ast::Expression::Modification { target, value }
                if target.parts.len() == 1
                    && &*target.parts[0].ident.text == "graphics" =>
            {
                Some(value)
            }
            _ => None,
        })
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let rumoca_session::parsing::ast::Expression::Array { elements, .. } = graphics_mod.as_ref() else {
        return Err(AstMutError::ValueParseFailed { value: text.to_string() });
    };
    elements
        .first()
        .cloned()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })
}

/// Parse a single `LunCoAnnotations.PlotNode(...)` record fragment as
/// it appears inside `__LunCo(plotNodes={...})`. Wraps it in a stub
/// class so the standard Modelica parser sees a well-formed input.
pub(crate) fn parse_plot_node_record(text: &str) -> Result<rumoca_session::parsing::ast::Expression, AstMutError> {
    let stub = format!(
        "model __LunCoFragment\nannotation(__LunCo(plotNodes={{{text}}}));\nend __LunCoFragment;\n"
    );
    let parsed = parse_stub_cached(&stub)
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let class = parsed
        .classes
        .get("__LunCoFragment")
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let lunco_call = class
        .annotation
        .first()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let rumoca_session::parsing::ast::Expression::ClassModification { modifications, .. } = lunco_call else {
        return Err(AstMutError::ValueParseFailed { value: text.to_string() });
    };
    let plot_nodes_mod = modifications
        .iter()
        .find_map(|m| match m {
            rumoca_session::parsing::ast::Expression::Modification { target, value }
                if target.parts.len() == 1
                    && &*target.parts[0].ident.text == "plotNodes" =>
            {
                Some(value)
            }
            _ => None,
        })
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })?;
    let rumoca_session::parsing::ast::Expression::Array { elements, .. } = plot_nodes_mod.as_ref() else {
        return Err(AstMutError::ValueParseFailed { value: text.to_string() });
    };
    elements
        .first()
        .cloned()
        .ok_or_else(|| AstMutError::ValueParseFailed { value: text.to_string() })
}

pub(crate) fn parse_experiment_expression(
    start_time: f64,
    stop_time: f64,
    tolerance: f64,
    interval: f64,
) -> Result<rumoca_session::parsing::ast::Expression, AstMutError> {
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

pub(crate) fn parse_placement_expression(placement: &pretty::Placement) -> Result<rumoca_session::parsing::ast::Expression, AstMutError> {
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
