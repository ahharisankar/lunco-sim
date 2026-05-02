//! Inspector panel — shows the canvas's current selection and lets the
//! user edit a component's modifications (parameter overrides).
//!
//! ## Architecture
//!
//! - **Reads** [`crate::ui::panels::canvas_diagram::CanvasDiagramState`] —
//!   the canvas owns selection state. The `primary()` selection's
//!   `Node(id)` is mapped through the scene to the Modelica instance
//!   name (`Node.origin`), then cross-referenced against the doc's AST
//!   to find the matching `Component` declaration.
//! - **Writes** via the unified [`crate::api_edits::ApplyModelicaOps`]
//!   Reflect event with [`crate::api_edits::ApiOp::SetParameter`]. The
//!   GUI never mutates state directly; per AGENTS.md §4.1 every edit
//!   goes through the same command surface an external API caller
//!   would use.
//!
//! ## What it shows
//!
//! For the selected component:
//! - instance name + declared type
//! - list of modifications (`R = 10`, `unit = "kg"`, …) with
//!   editable values
//!
//! Description strings, port lists, and the broader class structure
//! are deliberately not surfaced here — the agent-facing
//! `describe_model` API endpoint is the canonical place for those.
//! The inspector is the lightweight per-selection editor.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_canvas::SelectItem;
use lunco_workbench::{Panel, PanelId, PanelSlot};

use crate::api_edits::{ApiOp, ApplyModelicaOps};

pub struct InspectorPanel;

impl Panel for InspectorPanel {
    fn id(&self) -> PanelId {
        PanelId("modelica_diagram_inspector")
    }

    fn title(&self) -> String {
        "Inspector".into()
    }

    fn default_slot(&self) -> PanelSlot {
        PanelSlot::RightInspector
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        let warning = world
            .get_resource::<lunco_theme::Theme>()
            .map(|t| t.tokens.warning)
            .unwrap_or(egui::Color32::from_rgb(180, 150, 90));
        // ── Resolve target doc ───────────────────────────────────
        let active_doc = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document);
        let Some(doc_id) = active_doc else {
            placeholder(ui, "No active document.");
            return;
        };

        // Read-only state — derived from the document's origin (the
        // canonical source of truth; `ModelicaDocument::apply` enforces
        // the same invariant). We use it purely for UX (dim the text
        // editors) — even if a stray edit slipped through, the doc
        // would reject it. The defensive guard before firing
        // `ApplyModelicaOps` is a belt-and-braces gesture.
        let read_only = {
            let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
            registry
                .host(doc_id)
                .map(|h| h.document().is_read_only())
                .unwrap_or(false)
        };

        // ── Resolve the selected node ──────────────────────────
        //
        // Edges are not currently inspectable — we only surface
        // component-level edits. Plot nodes get a dedicated signal-
        // binding editor (see `render_plot_node_editor`); component
        // nodes go through the AST-driven modifications path below.
        let mut selection_kind = "none";
        let primary = world
            .get_resource::<crate::ui::panels::canvas_diagram::CanvasDiagramState>()
            .and_then(|cs| {
                let docstate = cs.get(Some(doc_id));
                let primary = docstate.canvas.selection.primary()?;
                match primary {
                    SelectItem::Node(node_id) => {
                        selection_kind = "node";
                        let node = docstate.canvas.scene.node(node_id)?;
                        Some((node_id, node.kind.clone(), node.origin.clone()))
                    }
                    SelectItem::Edge(_) => {
                        selection_kind = "edge";
                        None
                    }
                }
            });

        let Some((node_id, node_kind, node_origin)) = primary else {
            match selection_kind {
                "edge" => placeholder(ui, "Wire editing not supported yet."),
                _ => placeholder(ui, "Select a node on the canvas."),
            }
            return;
        };

        if node_kind.as_str() == lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND {
            render_plot_node_editor(ui, world, doc_id, node_id);
            return;
        }

        let Some(instance_name) = node_origin else {
            placeholder(ui, "Select a node on the canvas.");
            return;
        };

        // ── Resolve the active class on this doc ────────────────
        //
        // Mirrors `canvas_diagram::active_class_for_doc`: the
        // drilled-in pin wins when set, otherwise the document's
        // first non-package class (the same default the canvas
        // projects).
        let drilled_in = world
            .get_resource::<crate::ui::panels::canvas_diagram::DrilledInClassNames>()
            .and_then(|m| m.get(doc_id).map(str::to_string));

        // Scope the registry borrow tightly so we can free it before
        // any subsequent `world.commands()` calls.
        let (component_info, class) = {
            let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
            let Some(host) = registry.host(doc_id) else {
                placeholder(ui, "Document not in registry.");
                return;
            };
            let Some(ast) = host.document().ast().result.as_ref().ok().cloned() else {
                placeholder(ui, "Document has no parsed AST.");
                return;
            };
            let Some(class) = drilled_in.or_else(|| {
                crate::ast_extract::extract_model_name_from_ast(&ast)
            }) else {
                placeholder(ui, "Could not resolve target class.");
                return;
            };
            let short = class.rsplit('.').next().unwrap_or(&class).to_string();
            let Some(class_def) = crate::ast_extract::find_class_by_short_name(&ast, &short) else {
                placeholder(
                    ui,
                    &format!("Class `{short}` not found in document."),
                );
                return;
            };
            // Pick the matching component, project to a Reflect-friendly
            // owned struct so we can drop the AST borrow immediately.
            let Some(info) = crate::ast_extract::extract_components_for_class(class_def)
                .into_iter()
                .find(|c| c.name == instance_name)
            else {
                placeholder(
                    ui,
                    &format!(
                        "Selected node `{instance_name}` not declared in `{short}`."
                    ),
                );
                return;
            };
            (info, class)
        };

        // ── Render header ───────────────────────────────────────
        ui.add_space(4.0);
        ui.heading(&component_info.name);
        ui.label(
            egui::RichText::new(&component_info.type_name)
                .size(11.0)
                .color(egui::Color32::GRAY),
        );
        if !component_info.description.is_empty() {
            ui.label(&component_info.description);
        }
        if read_only {
            ui.label(
                egui::RichText::new(
                    "🔒 Read-only library tab — duplicate to workspace to edit.",
                )
                .italics()
                .color(warning),
            );
        }
        ui.separator();

        // ── Render modifications + collect edits ────────────────
        //
        // `text_edit_singleline` returns a Response; we apply the
        // edit on `lost_focus()` rather than `changed()` so a partial
        // value mid-typing doesn't fire a SetParameter on every
        // keystroke (which would push N undo entries onto the stack).
        let mut edits: Vec<(String, String)> = Vec::new();
        if component_info.modifications.is_empty() {
            ui.label(
                egui::RichText::new("No modifications declared. Edits will append new ones.")
                    .italics()
                    .color(egui::Color32::GRAY),
            );
        }
        // Stable order: sort by name so the inspector layout doesn't
        // jitter as the underlying HashMap iteration order shifts.
        let mut entries: Vec<(&String, &String)> = component_info.modifications.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        ui.collapsing("⚙ Modifications", |ui| {
            egui::Grid::new("modelica_inspector_mods")
                .num_columns(2)
                .spacing([10.0, 4.0])
                .show(ui, |ui| {
                    for (k, v) in entries {
                        ui.label(k);
                        let mut buf = v.clone();
                        // `add_enabled` disables the input on read-only
                        // tabs — egui dims it and ignores keystrokes,
                        // so the user clearly sees they can't edit.
                        let resp = ui.add_enabled(
                            !read_only,
                            egui::TextEdit::singleline(&mut buf),
                        );
                        if !read_only && resp.lost_focus() && buf != *v {
                            edits.push((k.clone(), buf));
                        }
                        ui.end_row();
                    }
                });
        });

        // ── Apply edits as a single batched event ───────────────
        // Defensive: even if a stray edit slipped through (shouldn't
        // happen given the disabled inputs above), don't fire ops on
        // a read-only doc.
        if !edits.is_empty() && !read_only {
            let ops: Vec<ApiOp> = edits
                .into_iter()
                .map(|(param, value)| ApiOp::SetParameter {
                    class: class.clone(),
                    component: instance_name.clone(),
                    param,
                    value,
                })
                .collect();
            world.commands().trigger(ApplyModelicaOps {
                doc: doc_id,
                ops,
            });
        }
    }
}

fn placeholder(ui: &mut egui::Ui, msg: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(20.0);
        ui.label(
            egui::RichText::new(msg)
                .italics()
                .color(egui::Color32::GRAY),
        );
    });
}

/// Inspector view for a selected `lunco.viz.plot` node — current
/// binding plus a clickable list of available signals. Picking a
/// signal swaps the node's `PlotNodeData` so the visual immediately
/// renders the new line on the next frame. The list is empty until
/// the active simulator has populated `SignalRegistry`; that case
/// shows a short hint instead of a blank panel.
fn render_plot_node_editor(
    ui: &mut egui::Ui,
    world: &mut World,
    doc_id: lunco_doc::DocumentId,
    node_id: lunco_canvas::NodeId,
) {
    use lunco_viz::kinds::canvas_plot_node::PlotNodeData;

    let current: PlotNodeData = world
        .get_resource::<crate::ui::panels::canvas_diagram::CanvasDiagramState>()
        .and_then(|cs| {
            cs.get(Some(doc_id))
                .canvas
                .scene
                .node(node_id)?
                .data
                .downcast_ref::<PlotNodeData>()
                .cloned()
        })
        .unwrap_or_default();

    ui.add_space(4.0);
    ui.heading("Plot");
    if current.signal_path.is_empty() {
        ui.label(
            egui::RichText::new("Unbound — pick a signal below.")
                .italics()
                .color(egui::Color32::GRAY),
        );
    } else {
        ui.label(
            egui::RichText::new(format!("Bound to: {}", current.signal_path))
                .small(),
        );
        if ui.button("Unbind").clicked() {
            apply_plot_binding(world, doc_id, node_id, 0, "");
        }
    }
    ui.separator();

    let sigs: Vec<(bevy::prelude::Entity, String)> = world
        .get_resource::<lunco_viz::SignalRegistry>()
        .map(|r| {
            let mut v: Vec<_> = r
                .iter_scalar()
                .map(|(s, _)| (s.entity, s.path.clone()))
                .collect();
            v.sort_by(|a, b| a.1.cmp(&b.1));
            v
        })
        .unwrap_or_default();

    if sigs.is_empty() {
        ui.label(
            egui::RichText::new("(no signals yet — run a simulation to bind)")
                .weak()
                .small(),
        );
        return;
    }

    let max_h = ui.ctx().screen_rect().height() * 0.6;
    egui::ScrollArea::vertical()
        .max_height(max_h)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (entity, path) in &sigs {
                let is_current = entity.to_bits() == current.entity
                    && path == &current.signal_path;
                let resp = ui.selectable_label(is_current, path);
                if resp.clicked() && !is_current {
                    apply_plot_binding(world, doc_id, node_id, entity.to_bits(), path);
                }
            }
        });
}

fn apply_plot_binding(
    world: &mut World,
    doc_id: lunco_doc::DocumentId,
    node_id: lunco_canvas::NodeId,
    entity_bits: u64,
    signal_path: &str,
) {
    use lunco_viz::kinds::canvas_plot_node::PlotNodeData;

    let payload = PlotNodeData {
        entity: entity_bits,
        signal_path: signal_path.to_string(),
        title: String::new(),
    };
    let data: lunco_canvas::NodeData = std::sync::Arc::new(payload);
    let mut state =
        world.resource_mut::<crate::ui::panels::canvas_diagram::CanvasDiagramState>();
    let docstate = state.get_mut(Some(doc_id));
    if let Some(node) = docstate.canvas.scene.node_mut(node_id) {
        node.data = data;
    }
}
