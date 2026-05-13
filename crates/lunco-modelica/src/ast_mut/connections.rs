//! Connection and port mutation helpers.

use rumoca_session::parsing::ast::{ClassDef, Equation, Expression};
use super::errors::AstMutError;
use super::parsing::{parse_connect_equation_fragment, FRAGMENT_CLASS_NAME};
use super::util::{matches_port_ref, expression_is_line_call, extract_points_named_argument, named_arg_name, ref_is_simple, fmt_f64};
use crate::pretty;

/// Append a `connect(...)` equation to a class.
pub fn add_connection(
    class: &mut ClassDef,
    eq: &pretty::ConnectEquation,
) -> Result<(), AstMutError> {
    let new_eq = parse_connect_equation_fragment(eq)?;
    class.equations.push(new_eq);
    Ok(())
}

/// Remove a `connect(...)` equation matching `(from, to)` PortRefs.
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
            Equation::Connect { lhs, rhs, .. }
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

/// Swap `lhs`/`rhs` of a matching `connect(...)` equation.
pub fn reverse_connection(
    class: &mut ClassDef,
    from: &pretty::PortRef,
    to: &pretty::PortRef,
) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let mut matched = false;
    for eq in class.equations.iter_mut() {
        if let Equation::Connect { lhs, rhs, .. } = eq {
            if matches_port_ref(lhs, from) && matches_port_ref(rhs, to) {
                std::mem::swap(lhs, rhs);
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

/// Set or clear the `annotation(Line(points={...}))` on a
/// `connect(...)` equation matching `(from, to)`.
pub fn set_connection_line(
    class: &mut ClassDef,
    from: &pretty::PortRef,
    to: &pretty::PortRef,
    points: &[(f32, f32)],
) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();

    let new_annotation: Vec<Expression> = if points.is_empty() {
        Vec::new()
    } else {
        let stub_eq = pretty::ConnectEquation {
            from: from.clone(),
            to: to.clone(),
            line: Some(pretty::Line { points: points.to_vec() }),
        };
        let parsed = parse_connect_equation_fragment(&stub_eq)?;
        match parsed {
            Equation::Connect { annotation, .. } => annotation,
            _ => return Err(AstMutError::ValueParseFailed { value: "connect annotation".into() }),
        }
    };

    let mut matched = false;
    for eq in class.equations.iter_mut() {
        if let Equation::Connect { lhs, rhs, annotation } = eq {
            if matches_port_ref(lhs, from) && matches_port_ref(rhs, to) {
                if points.is_empty() {
                    annotation.retain(|e| !expression_is_line_call(e));
                } else {
                    let new_points_arg =
                        extract_points_named_argument(&new_annotation);
                    let mut updated = false;
                    if let Some(new_arg) = new_points_arg {
                        for entry in annotation.iter_mut() {
                            if let Expression::FunctionCall {
                                comp,
                                args,
                            } = entry
                            {
                                if !ref_is_simple(comp, "Line") {
                                    continue;
                                }
                                let mut replaced = false;
                                for a in args.iter_mut() {
                                    if named_arg_name(a) == Some("points") {
                                        *a = new_arg.clone();
                                        replaced = true;
                                        break;
                                    }
                                }
                                if !replaced {
                                    args.insert(0, new_arg.clone());
                                }
                                updated = true;
                                break;
                            }
                        }
                    }
                    if !updated {
                        *annotation = new_annotation.clone();
                    }
                }
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

/// Set or clear individual `Line(...)` annotation fields on a
/// `connect(...)` equation matching `(from, to)`.
pub fn set_connection_line_style(
    class: &mut ClassDef,
    from: &pretty::PortRef,
    to: &pretty::PortRef,
    color: Option<[u8; 3]>,
    thickness: Option<f64>,
    smooth_bezier: Option<bool>,
) -> Result<(), AstMutError> {
    let class_name = class.name.text.to_string();
    let mut stub_args: Vec<String> = Vec::new();
    stub_args.push("points={{0,0},{0,0}}".into());
    if let Some([r, g, b]) = color {
        stub_args.push(format!("color={{{},{},{}}}", r, g, b));
    }
    if let Some(t) = thickness {
        stub_args.push(format!("thickness={}", fmt_f64(t)));
    }
    if let Some(s) = smooth_bezier {
        let v = if s { "Smooth.Bezier" } else { "Smooth.None" };
        stub_args.push(format!("smooth={}", v));
    }
    let stub_body = format!(
        "connect(a.b, c.d) annotation(Line({}));\n",
        stub_args.join(", ")
    );
    let stub = format!(
        "model {FRAGMENT_CLASS_NAME}\nequation\n  {stub_body}end {FRAGMENT_CLASS_NAME};\n",
    );
    let parsed = super::parsing::parse_stub_cached(&stub).ok_or_else(|| {
        AstMutError::ValueParseFailed { value: stub_body.clone() }
    })?;
    let parsed_cls = parsed.classes.get(FRAGMENT_CLASS_NAME).ok_or_else(|| {
        AstMutError::ValueParseFailed { value: stub_body.clone() }
    })?;
    let parsed_eq = parsed_cls.equations.first().ok_or_else(|| {
        AstMutError::ValueParseFailed { value: stub_body.clone() }
    })?;
    let parsed_annotation = match parsed_eq {
        Equation::Connect { annotation, .. } => annotation,
        _ => return Err(AstMutError::ValueParseFailed { value: stub_body }),
    };

    let mut matched = false;
    for eq in class.equations.iter_mut() {
        if let Equation::Connect { lhs, rhs, annotation } = eq {
            if !(matches_port_ref(lhs, from) && matches_port_ref(rhs, to)) {
                continue;
            }
            let line_idx = annotation
                .iter()
                .position(|e| expression_is_line_call(e));
            if line_idx.is_none() {
                if let Some(line_expr) =
                    parsed_annotation.iter().find(|e| expression_is_line_call(e))
                {
                    annotation.push(line_expr.clone());
                }
            }
            let line_idx = annotation
                .iter()
                .position(|e| expression_is_line_call(e))
                .ok_or_else(|| AstMutError::ValueParseFailed {
                    value: "Line entry missing after splice".into(),
                })?;
            let names_to_patch: Vec<&str> = {
                let mut v: Vec<&str> = Vec::new();
                if color.is_some() { v.push("color"); }
                if thickness.is_some() { v.push("thickness"); }
                if smooth_bezier.is_some() { v.push("smooth"); }
                v
            };
            if let Expression::FunctionCall { args, .. } = &mut annotation[line_idx] {
                for name in names_to_patch {
                    let Some(new_arg) = parsed_annotation
                        .iter()
                        .filter_map(|e| match e {
                            Expression::FunctionCall { comp, args } if ref_is_simple(comp, "Line") => Some(args),
                            _ => None,
                        })
                        .flatten()
                        .find(|a| named_arg_name(a) == Some(name))
                        .cloned()
                    else { continue };
                    let mut replaced = false;
                    for a in args.iter_mut() {
                        if named_arg_name(a) == Some(name) {
                            *a = new_arg.clone();
                            replaced = true;
                            break;
                        }
                    }
                    if !replaced {
                        args.push(new_arg);
                    }
                }
            }
            matched = true;
            break;
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
