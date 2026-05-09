//! Modelica-side adapter to the canonical Twin journal.
//!
//! `lunco-twin-journal` is generic and stores op payloads as
//! `serde_json::Value`. This module produces structured *summaries* of
//! [`ModelicaOp`]s and records them in the journal alongside their
//! inverses.
//!
//! ## Why summaries, not full op serialization
//!
//! [`ModelicaOp`] carries domain-specific structures (`ComponentDecl`,
//! `Placement`, `ConnectEquation`, `LunCoPlotNodeSpec`, …) that are not
//! `Serialize` today. Deriving `Serialize` end-to-end is a follow-up
//! when the full-replay / CRDT path matures. For the foundation the
//! journal needs *enough* information to:
//!
//! - render a meaningful row in the JournalLog panel,
//! - filter by document / author / scope for undo,
//! - feed audit / debug / future telemetry consumers.
//!
//! A flat `{kind, class, name, …}` Value carries that signal. When the
//! op set grows full `Serialize`, this adapter becomes a one-liner
//! (`serde_json::to_value(op)`).
//!
//! ## Author tagging
//!
//! Author defaults to [`AuthorTag::local_user`]. Future entry points
//! (HTTP API observers, agent scripts) construct their own
//! [`AuthorTag::for_tool`] before calling [`record_op_summary`].

use serde_json::{json, Value};

use crate::document::ModelicaOp;

/// Build a structured summary of a [`ModelicaOp`] for the journal.
///
/// Each variant produces a JSON object with a `kind` discriminant and
/// the key fields a UI / audit layer cares about — class name, instance
/// name, parameter, etc. Bulky payloads (full source text, large graphic
/// specs) are summarised as length / kind rather than embedded verbatim.
pub fn summarize_op(op: &ModelicaOp) -> Value {
    match op {
        ModelicaOp::ReplaceSource { new } => json!({
            "kind": "ReplaceSource",
            "len": new.len(),
        }),
        ModelicaOp::EditText { range, replacement } => json!({
            "kind": "EditText",
            "range": [range.start, range.end],
            "replacement_len": replacement.len(),
        }),
        ModelicaOp::AddComponent { class, decl } => json!({
            "kind": "AddComponent",
            "class": class,
            "name": decl.name,
            "type": decl.type_name,
        }),
        ModelicaOp::AddConnection { class, eq } => json!({
            "kind": "AddConnection",
            "class": class,
            "from": format!("{}.{}", eq.from.component, eq.from.port),
            "to": format!("{}.{}", eq.to.component, eq.to.port),
        }),
        ModelicaOp::RemoveComponent { class, name } => json!({
            "kind": "RemoveComponent",
            "class": class,
            "name": name,
        }),
        ModelicaOp::RemoveConnection { class, from, to } => json!({
            "kind": "RemoveConnection",
            "class": class,
            "from": format!("{}.{}", from.component, from.port),
            "to": format!("{}.{}", to.component, to.port),
        }),
        ModelicaOp::SetPlacement { class, name, .. } => json!({
            "kind": "SetPlacement",
            "class": class,
            "name": name,
        }),
        ModelicaOp::SetParameter { class, component, param, value } => json!({
            "kind": "SetParameter",
            "class": class,
            "component": component,
            "param": param,
            "value": value,
        }),
        ModelicaOp::AddPlotNode { class, plot } => json!({
            "kind": "AddPlotNode",
            "class": class,
            "signal": plot.signal,
        }),
        ModelicaOp::RemovePlotNode { class, signal_path } => json!({
            "kind": "RemovePlotNode",
            "class": class,
            "signal": signal_path,
        }),
        ModelicaOp::SetPlotNodeExtent { class, signal_path, .. } => json!({
            "kind": "SetPlotNodeExtent",
            "class": class,
            "signal": signal_path,
        }),
        ModelicaOp::SetPlotNodeTitle { class, signal_path, title } => json!({
            "kind": "SetPlotNodeTitle",
            "class": class,
            "signal": signal_path,
            "title": title,
        }),
        ModelicaOp::SetDiagramTextExtent { class, index, .. } => json!({
            "kind": "SetDiagramTextExtent",
            "class": class,
            "index": index,
        }),
        ModelicaOp::SetDiagramTextString { class, index, text } => json!({
            "kind": "SetDiagramTextString",
            "class": class,
            "index": index,
            "text": text,
        }),
        ModelicaOp::RemoveDiagramText { class, index } => json!({
            "kind": "RemoveDiagramText",
            "class": class,
            "index": index,
        }),
        ModelicaOp::AddClass { parent, name, kind, partial, .. } => json!({
            "kind": "AddClass",
            "parent": parent,
            "name": name,
            "class_kind": format!("{:?}", kind),
            "partial": partial,
        }),
        ModelicaOp::RemoveClass { qualified } => json!({
            "kind": "RemoveClass",
            "qualified": qualified,
        }),
        ModelicaOp::AddShortClass { parent, name, kind, base, .. } => json!({
            "kind": "AddShortClass",
            "parent": parent,
            "name": name,
            "class_kind": format!("{:?}", kind),
            "base": base,
        }),
        ModelicaOp::AddVariable { class, decl } => json!({
            "kind": "AddVariable",
            "class": class,
            "name": decl.name,
            "type": decl.type_name,
        }),
        ModelicaOp::RemoveVariable { class, name } => json!({
            "kind": "RemoveVariable",
            "class": class,
            "name": name,
        }),
        ModelicaOp::AddEquation { class, .. } => json!({
            "kind": "AddEquation",
            "class": class,
        }),
        ModelicaOp::AddIconGraphic { class, .. } => json!({
            "kind": "AddIconGraphic",
            "class": class,
        }),
        ModelicaOp::AddDiagramGraphic { class, .. } => json!({
            "kind": "AddDiagramGraphic",
            "class": class,
        }),
        ModelicaOp::SetExperimentAnnotation { class, start_time, stop_time, tolerance, interval } => json!({
            "kind": "SetExperimentAnnotation",
            "class": class,
            "start_time": start_time,
            "stop_time": stop_time,
            "tolerance": tolerance,
            "interval": interval,
        }),
    }
}

