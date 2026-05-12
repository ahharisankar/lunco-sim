//! Text and annotation extraction helpers for Modelica documentation.

pub fn extract_documentation(
    annotations: &[rumoca_session::parsing::ast::Expression],
) -> (Option<String>, Option<String>) {
    use rumoca_session::parsing::ast::{Expression, TerminalType};
    let call = annotations.iter().find(|e| match e {
        Expression::FunctionCall { comp, .. } | Expression::ClassModification { target: comp, .. } => {
            comp.parts
                .first()
                .map(|p| p.ident.text.as_ref() == "Documentation")
                .unwrap_or(false)
        }
        _ => false,
    });
    let Some(call) = call else { return (None, None) };
    let args: &[Expression] = match call {
        Expression::FunctionCall { args, .. } => args.as_slice(),
        Expression::ClassModification { modifications, .. } => modifications.as_slice(),
        _ => return (None, None),
    };
    let str_arg = |name: &str| -> Option<String> {
        for a in args {
            let (arg_name, value) = match a {
                Expression::NamedArgument { name, value } => {
                    (name.text.as_ref(), value.as_ref())
                }
                Expression::Modification { target, value } => (
                    target.parts.first().map(|p| p.ident.text.as_ref()).unwrap_or(""),
                    value.as_ref(),
                ),
                _ => continue,
            };
            if arg_name != name {
                continue;
            }
            if let Expression::Terminal { terminal_type: TerminalType::String, token } = value {
                let raw = token.text.as_ref();
                let inner = raw.trim_start_matches('"').trim_end_matches('"');
                return Some(unescape_modelica_string(inner));
            }
        }
        None
    };
    (str_arg("info"), str_arg("revisions"))
}

pub fn unescape_modelica_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                match n {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '\'' => out.push('\''),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else {
                out.push('\\');
            }
        } else {
            out.push(c);
        }
    }
    out
}
