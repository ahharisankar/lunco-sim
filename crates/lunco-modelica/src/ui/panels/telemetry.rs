//! Telemetry panel — model parameters, inputs, and variable plotting toggles.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_workbench::{Panel, PanelId, PanelSlot};
use std::collections::HashMap;

use crate::ui::WorkbenchState;
use crate::ui::viz::{is_signal_plotted, set_signal_plotted};
use crate::{ModelicaModel, ModelicaChannels, ModelicaCommand};

/// Look up a description string with a leaf-name fallback. Runtime
/// variable names are fully-qualified (e.g. `"engine.thrust"`) but
/// `extract_descriptions` keys by the local component name
/// (`"thrust"` declared inside `model Engine`). Try the full name
/// first — covers top-level components of the target class — then
/// fall back to the last dotted segment.
fn lookup_desc<'a>(
    descriptions: &'a HashMap<String, String>,
    name: &str,
) -> Option<&'a String> {
    if let Some(d) = descriptions.get(name) {
        return Some(d);
    }
    let leaf = name.rsplit('.').next().unwrap_or(name);
    if leaf != name {
        descriptions.get(leaf)
    } else {
        None
    }
}

/// Same leaf-name fallback as `lookup_desc`, applied to the
/// `(min, max)` bounds map. The AST extractor keys bounds by leaf
/// component name (`opening`) because bound declarations live inside
/// the component class; the runtime queries by fully-qualified
/// instance path (`valve.opening`). Try the qualified name first
/// (handles top-level components of the active class) then fall back
/// to the leaf.
fn lookup_bounds(
    bounds: &HashMap<String, (Option<f64>, Option<f64>)>,
    name: &str,
) -> (Option<f64>, Option<f64>) {
    if let Some(b) = bounds.get(name) {
        return *b;
    }
    let leaf = name.rsplit('.').next().unwrap_or(name);
    if leaf != name {
        if let Some(b) = bounds.get(leaf) {
            return *b;
        }
    }
    (None, None)
}

/// Render `body` inside a fixed-height region with a draggable
/// horizontal divider beneath it. The divider lets the user grow /
/// shrink the region at the expense of whatever follows it in the
/// panel — Telemetry uses this to share vertical space between
/// Parameters, Inputs, and the Variables list. Height persists
/// across sessions in egui memory keyed by `id`.
///
/// `egui::Resize` ships a tiny corner grip that's invisible to most
/// users; this gives them a wide, painted bar with a `ResizeRow`
/// cursor on hover — the affordance professional UIs use.
fn resizable_v_section<R>(
    ui: &mut egui::Ui,
    id: &str,
    default_h: f32,
    body: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let id = ui.make_persistent_id(id);
    let mut h = ui
        .memory_mut(|m| m.data.get_persisted::<f32>(id))
        .unwrap_or(default_h);
    let avail_w = ui.available_width();
    let result = ui
        .allocate_ui_with_layout(
            egui::vec2(avail_w, h),
            egui::Layout::top_down(egui::Align::Min),
            body,
        )
        .inner;
    // Drag handle — a 6 px tall horizontal strip with a centred
    // "grip" line so the affordance is visible even at rest.
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(avail_w, 6.0), egui::Sense::drag());
    let visuals = ui.visuals();
    let stroke_color = if resp.hovered() || resp.dragged() {
        visuals.selection.bg_fill
    } else {
        visuals.widgets.inactive.bg_stroke.color
    };
    let y = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + 8.0, y),
            egui::pos2(rect.right() - 8.0, y),
        ],
        egui::Stroke::new(2.0, stroke_color),
    );
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeRow);
    }
    if resp.dragged() {
        h = (h + resp.drag_delta().y).clamp(40.0, 800.0);
        ui.memory_mut(|m| m.data.insert_persisted(id, h));
    }
    result
}

/// Telemetry panel — model parameters, inputs, and variable plotting toggles.
pub struct TelemetryPanel;

impl Panel for TelemetryPanel {
    fn id(&self) -> PanelId { PanelId("modelica_inspector") }
    fn title(&self) -> String { "📊 Telemetry".into() }
    fn default_slot(&self) -> PanelSlot { PanelSlot::RightInspector }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        // Fix selection leakage
        ui.style_mut().interaction.selectable_labels = false;
        let muted = world
            .get_resource::<lunco_theme::Theme>()
            .map(|t| t.tokens.text_subdued)
            .unwrap_or(egui::Color32::from_rgb(140, 140, 160));

        // Component inspector — when one or more nodes are selected
        // on the active diagram, show their parameters and let the
        // user edit them. Works pre- and post-compile; edits go
        // through the same `SetParameter` op the canvas drag flow
        // uses, so undo / re-projection / journaling stay consistent.
        render_selected_components_inspector(ui, world, muted);

        // Resolve the entity to display: explicit pin (`selected_entity`)
        // wins so the future "Pin to a specific model" UX stays
        // possible, otherwise follow the active document — same
        // lookup any doc-scoped panel uses. The previous form read
        // `selected_entity` only and was clobbered on every Compile,
        // stranding the user looking at whichever model was compiled
        // last regardless of which tab they had focused.
        let (entity, has_data) = {
            let pinned = world
                .get_resource::<WorkbenchState>()
                .and_then(|s| s.selected_entity);
            let resolved = pinned.or_else(|| crate::ui::state::active_simulator(world));
            let has = resolved
                .map(|e| world.get::<ModelicaModel>(e).is_some())
                .unwrap_or(false);
            (resolved, has)
        };

        let Some(entity) = entity else {
            ui.label("No model selected.");
            return;
        };
        if !has_data {
            ui.label("Model not found.");
            return;
        }

        // Read model snapshot for display. Parameter editing lives in
        // `render_selected_components_inspector` (op-pipeline based);
        // the panel only reads runtime values here.
        let (model_name, is_paused, current_time, inputs, descriptions, parameter_bounds) = {
            if let Some(model) = world.get::<ModelicaModel>(entity) {
                (model.model_name.clone(), model.paused, model.current_time,
                 model.inputs.clone(),
                 model.descriptions.clone(), model.parameter_bounds.clone())
            } else {
                ui.label("Model not found.");
                return;
            }
        };

        let display_name = world.query::<Option<&Name>>().get(world, entity).ok().flatten()
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "Unnamed Model".to_string());

        ui.heading(format!("{display_name} ({model_name})"));

        // Play/Pause
        ui.horizontal(|ui| {
            if is_paused {
                if ui.button("▶ Play").clicked() {
                    if let Ok(mut m) = world.query::<&mut ModelicaModel>().get_mut(world, entity) {
                        m.paused = false;
                    }
                }
            } else {
                if ui.button("⏸ Pause").clicked() {
                    if let Ok(mut m) = world.query::<&mut ModelicaModel>().get_mut(world, entity) {
                        m.paused = true;
                    }
                }
            }
            ui.label(format!("Time: {current_time:.4} s"));

            ui.add_space(ui.available_width() - 70.0);
            if ui.button("🔄 Reset").clicked() {
                let sid = if let Ok(mut m) = world.query::<&mut ModelicaModel>().get_mut(world, entity) {
                    m.session_id += 1;
                    m.is_stepping = true;
                    m.current_time = 0.0;
                    m.last_step_time = 0.0;
                    Some(m.session_id)
                } else { None };
                if let (Some(sid), Some(channels)) = (sid, world.get_resource::<ModelicaChannels>()) {
                    let _ = channels.tx.send(ModelicaCommand::Reset { entity, session_id: sid });
                }
                // The worker's Reset handler pushes a fresh set of
                // samples into `SignalRegistry`; clearing per-signal
                // history is handled there.
            }
        });
        ui.separator();

        // Inputs
        if !inputs.is_empty() {
            ui.label("Inputs (Real-time):");
            resizable_v_section(ui, "inputs_height", 120.0, |ui| {
                egui::ScrollArea::vertical().id_salt("inputs_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    let mut input_keys: Vec<_> = inputs.keys().cloned().collect();
                    input_keys.sort();
                    egui::Grid::new("inputs_grid")
                        .num_columns(2)
                        .striped(true)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            for key in input_keys {
                                let val = inputs.get(&key).copied().unwrap_or(0.0);
                                let label = egui::Label::new(format!("{key}"))
                                    .sense(egui::Sense::hover());
                                let resp = ui.add(label);
                                if let Some(desc) = lookup_desc(&descriptions, &key) {
                                    resp.on_hover_text(desc);
                                }
                                let mut v = val;
                                let (mn, mx) = lookup_bounds(&parameter_bounds, &key);
                                let avail = ui.available_width().max(60.0);
                                ui.add_sized(
                                    [avail, 20.0],
                                    egui::DragValue::new(&mut v)
                                        .speed(0.1)
                                        .fixed_decimals(2)
                                        .range(
                                            mn.unwrap_or(f64::NEG_INFINITY)
                                                ..=mx.unwrap_or(f64::INFINITY),
                                        ),
                                );
                                ui.end_row();
                                if (v - val).abs() > 1e-10 {
                                    if let Ok(mut m) = world.query::<&mut ModelicaModel>().get_mut(world, entity) {
                                        if let Some(inp) = m.inputs.get_mut(&key) { *inp = v; }
                                    }
                                }
                            }
                        });
                });
            });
        }

        // Variables (Toggle to Plot).
        //
        // Checkboxes read / write the default Modelica plot's
        // `VisualizationConfig.inputs` directly — no shadow state,
        // no per-frame sync. Toggling here instantly shows/hides the
        // variable in the Graphs panel since both read the same
        // config.
        ui.label("Variables (Toggle to Plot):");
        egui::ScrollArea::vertical().id_salt("telemetry_scroll").show(ui, |ui| {
            let (model_vars, model_inputs) = if let Some(m) = world.get::<ModelicaModel>(entity) {
                (m.variables.keys().cloned().collect::<Vec<_>>(),
                 m.inputs.keys().cloned().collect::<Vec<_>>())
            } else {
                (Vec::new(), Vec::new())
            };

            // Read plotted-set from the viz registry. Clone once so
            // we don't reborrow the resource inside the loop.
            let plotted: std::collections::HashSet<String> = world
                .get_resource::<lunco_viz::VisualizationRegistry>()
                .and_then(|r| r.get(crate::ui::viz::DEFAULT_MODELICA_GRAPH))
                .map(|cfg| cfg.inputs.iter()
                    .filter(|b| b.source.entity == entity)
                    .map(|b| b.source.path.clone())
                    .collect())
                .unwrap_or_default();

            let mut all_names: Vec<_> = model_vars;
            all_names.extend(model_inputs);
            all_names.sort();
            all_names.dedup();

            for name in all_names {
                let mut is_plotted = plotted.contains(&name);
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut is_plotted, "").changed() {
                        if let Some(mut reg) =
                            world.get_resource_mut::<lunco_viz::VisualizationRegistry>()
                        {
                            set_signal_plotted(
                                &mut reg,
                                lunco_viz::SignalRef::new(entity, name.clone()),
                                is_plotted,
                            );
                        }
                    }
                    let label = egui::Label::new(&name).sense(egui::Sense::hover());
                    let resp = ui.add(label);
                    if let Some(desc) = lookup_desc(&descriptions, &name).filter(|d| !d.trim().is_empty()) {
                        // Hover for the full string (can be long),
                        // plus a muted inline preview so users who
                        // never hover still see the hint exists.
                        resp.on_hover_text(desc);
                        ui.label(
                            egui::RichText::new(desc.trim())
                                .italics()
                                .color(muted)
                                .size(11.0),
                        )
                        .on_hover_text(desc);
                    }
                });
                let _ = is_signal_plotted; // re-export available for future UIs
            }
        });

        // Auto-Fit button was here but moved to the Graphs panel's own
        // toolbar — users couldn't find it buried at the bottom of
        // Telemetry. Telemetry now does parameters / inputs / variable
        // toggles only; graph-axis controls live on the graph itself.
    }
}

/// Render the parameter inspector for component nodes selected on
/// the active diagram. One header per node (instance — class), then
/// editable rows for every parameter on that class. Edits dispatch
/// `ModelicaOp::SetParameter` through the canvas's apply_ops pipeline
/// so they show up in the source, the projection, undo history, and
/// — once compiled — the simulator.
fn render_selected_components_inspector(
    ui: &mut egui::Ui,
    world: &mut World,
    muted: egui::Color32,
) {
    use crate::document::ModelicaOp;
    use crate::ui::panels::canvas_diagram::{
        active_class_for_doc, active_doc_from_world, apply_ops_public,
        CanvasDiagramState, IconNodeData,
    };

    let Some(doc_id) = active_doc_from_world(world) else { return };
    // Snapshot the selected nodes' (id, instance, class, params) up
    // front so we can release the canvas-state borrow before issuing
    // commands.queue / apply_ops_public mutations.
    struct NodeRow {
        instance: String,
        qualified_type: String,
        // (param_name, current_value).
        parameters: Vec<(String, String)>,
    }
    let rows: Vec<NodeRow> = {
        let Some(state) = world.get_resource::<CanvasDiagramState>() else {
            return;
        };
        let docstate = state.get(Some(doc_id));
        let scene = &docstate.canvas.scene;
        let selection = &docstate.canvas.selection;
        selection
            .iter()
            .filter_map(|item| match *item {
                lunco_canvas::SelectItem::Node(id) => {
                    let node = scene.node(id)?;
                    let icon = node.data.downcast_ref::<IconNodeData>()?;
                    Some(NodeRow {
                        instance: node.label.clone(),
                        qualified_type: icon.qualified_type.clone(),
                        parameters: icon.parameters.clone(),
                    })
                }
                _ => None,
            })
            .collect()
    };
    if rows.is_empty() {
        return;
    }

    let editing_class = active_class_for_doc(world, doc_id);

    egui::CollapsingHeader::new(format!(
        "🧩 Selected components ({})",
        rows.len()
    ))
    .default_open(true)
    .show(ui, |ui| {
        if editing_class.is_none() {
            ui.label(
                egui::RichText::new(
                    "No active class — open a model class on the canvas to edit parameters.",
                )
                .size(11.0)
                .color(muted),
            );
            return;
        }
        let class = editing_class.expect("class is Some by the branch above");
        // Per-node block. CollapsingHeader so multi-select stays
        // navigable on tall lists.
        for row in &rows {
            let leaf_type = row
                .qualified_type
                .rsplit('.')
                .next()
                .unwrap_or(&row.qualified_type)
                .to_string();
            egui::CollapsingHeader::new(
                egui::RichText::new(format!("{} — {}", row.instance, leaf_type)).strong(),
            )
            .id_salt(("selected_component", row.instance.as_str()))
            .default_open(true)
            .show(ui, |ui| {
                if row.parameters.is_empty() {
                    ui.label(
                        egui::RichText::new("(no parameters)")
                            .size(11.0)
                            .color(muted)
                            .italics(),
                    );
                    return;
                }
                // Two-pass: collect edits during the row loop, apply
                // after the immutable borrow on `rows` is done. Using
                // a String value keeps the editor general — Modelica
                // params can be Real / Integer / Boolean / enumeration,
                // and `SetParameter` accepts a textual replacement.
                let mut edits: Vec<(String, String)> = Vec::new();
                for (name, value) in &row.parameters {
                    let mut buf = value.clone();
                    ui.horizontal(|ui| {
                        ui.label(format!("{name:14}"));
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut buf)
                                .desired_width(120.0),
                        );
                        if resp.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            if buf != *value {
                                edits.push((name.clone(), buf.clone()));
                            }
                        } else if resp.lost_focus() && buf != *value {
                            edits.push((name.clone(), buf.clone()));
                        }
                    });
                }
                for (param, value) in edits {
                    apply_ops_public(
                        world,
                        doc_id,
                        vec![ModelicaOp::SetParameter {
                            class: class.clone(),
                            component: row.instance.clone(),
                            param,
                            value,
                        }],
                    );
                }
            });
        }
    });
    ui.separator();
}
