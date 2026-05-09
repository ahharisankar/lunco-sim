//! Telemetry panel — model parameters, inputs, and variable plotting toggles.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_workbench::{Panel, PanelId, PanelSlot};

use crate::ui::WorkbenchState;
use crate::ui::viz::{is_signal_plotted, set_signal_plotted};
use crate::{ModelicaModel, ModelicaChannels, ModelicaCommand};

/// Per-input metadata snapshot — built once per render from
/// [`crate::index::ModelicaIndex`] so the grid loop doesn't reborrow
/// the document registry per row. Description and bounds resolve via
/// [`crate::index::ModelicaIndex::find_component_by_leaf`].
struct InputRow {
    name: String,
    value: f64,
    description: Option<String>,
    min: Option<f64>,
    max: Option<f64>,
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
        render_active_class_parameters(ui, world, muted);

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
        let (model_name, is_paused, current_time, inputs, doc_id) = {
            if let Some(model) = world.get::<ModelicaModel>(entity) {
                (model.model_name.clone(), model.paused, model.current_time,
                 model.inputs.clone(), model.document)
            } else {
                ui.label("Model not found.");
                return;
            }
        };

        // Snapshot per-input metadata from the document index so the
        // inputs grid below can render tooltips and bound-clamped
        // sliders without reborrowing the registry per row.
        let input_rows: Vec<InputRow> = {
            let mut sorted: Vec<(String, f64)> = inputs.into_iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let registry = world.get_resource::<crate::ui::ModelicaDocumentRegistry>();
            let index_ref = registry
                .and_then(|r| r.host(doc_id))
                .map(|h| h.document().index());
            sorted
                .into_iter()
                .map(|(name, value)| {
                    let entry =
                        index_ref.and_then(|idx| idx.find_component_by_leaf(&name));
                    InputRow {
                        description: entry
                            .map(|e| e.description.clone())
                            .filter(|s| !s.is_empty()),
                        min: entry
                            .and_then(|e| e.modifications.get("min").and_then(|s| s.parse().ok())),
                        max: entry
                            .and_then(|e| e.modifications.get("max").and_then(|s| s.parse().ok())),
                        name,
                        value,
                    }
                })
                .collect()
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
        if !input_rows.is_empty() {
            ui.label("Inputs (Real-time):");
            resizable_v_section(ui, "inputs_height", 120.0, |ui| {
                egui::ScrollArea::vertical().id_salt("inputs_scroll").auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("inputs_grid")
                        .num_columns(2)
                        .striped(true)
                        .spacing([8.0, 4.0])
                        .show(ui, |ui| {
                            for row in &input_rows {
                                let label = egui::Label::new(row.name.clone())
                                    .sense(egui::Sense::hover());
                                let resp = ui.add(label);
                                if let Some(desc) = &row.description {
                                    resp.on_hover_text(desc);
                                }
                                let mut v = row.value;
                                let avail = ui.available_width().max(60.0);
                                ui.add_sized(
                                    [avail, 20.0],
                                    egui::DragValue::new(&mut v)
                                        .speed(0.1)
                                        .fixed_decimals(2)
                                        .range(
                                            row.min.unwrap_or(f64::NEG_INFINITY)
                                                ..=row.max.unwrap_or(f64::INFINITY),
                                        ),
                                );
                                ui.end_row();
                                if (v - row.value).abs() > 1e-10 {
                                    if let Ok(mut m) = world.query::<&mut ModelicaModel>().get_mut(world, entity) {
                                        if let Some(inp) = m.inputs.get_mut(&row.name) { *inp = v; }
                                    }
                                }
                            }
                        });
                });
            });
        }

        // Variables (Toggle to Plot).
        //
        // Checkboxes write to TWO things in lockstep so this is the
        // single place to pick variables for plotting:
        //   1. `VisualizationConfig.inputs` — drives the live cosim
        //      plot in Graphs (ticked vars stream samples there).
        //   2. `ExperimentVisibility.picked_vars` — drives the
        //      experiment plot in Graphs (ticked vars are drawn for
        //      every visible Fast Run).
        // Names match between the two paths (both come from the same
        // model compile), so one tick = one curve per source.
        //
        // Filter + group: search field collapses noise on big models;
        // collapsing-headers per top-level component group keep the
        // panel scannable.
        ui.horizontal(|ui| {
            ui.label("Variables");
            ui.weak("(toggle to plot)");
        });

        // Filter input lives on ExperimentVisibility — same resource
        // that already holds `picked_vars`, no new state.
        let mut filter_text = world
            .get_resource::<crate::ui::panels::experiments::ExperimentVisibility>()
            .map(|v| v.var_filter.clone())
            .unwrap_or_default();
        let mut filter_changed = false;
        ui.horizontal(|ui| {
            ui.label("🔍");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut filter_text)
                    .hint_text("filter…")
                    .desired_width(160.0),
            );
            if resp.changed() {
                filter_changed = true;
            }
            if ui.small_button("✕").on_hover_text("Clear filter").clicked() {
                filter_text.clear();
                filter_changed = true;
            }
        });
        if filter_changed {
            if let Some(mut vis) = world
                .get_resource_mut::<crate::ui::panels::experiments::ExperimentVisibility>()
            {
                vis.var_filter = filter_text.clone();
            }
        }
        let filter_lower = filter_text.to_ascii_lowercase();

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

            // Picked-for-experiments set, snapshotted once. Read from
            // the active plot panel (most-recently-rendered) so the
            // checkboxes reflect that plot's picks.
            let active_plot = world
                .get_resource::<crate::ui::panels::experiments::ActivePlot>()
                .copied()
                .unwrap_or_default()
                .or_default();
            let picked_exp: std::collections::BTreeSet<String> = world
                .get_resource::<crate::ui::panels::experiments::PlotPanelStates>()
                .map(|s| s.picked(active_plot))
                .unwrap_or_default();

            // Variables sourced from completed experiments — surface
            // them even when there's no live cosim entity yet.
            let exp_vars: std::collections::BTreeSet<String> =
                crate::ui::panels::experiments::all_experiment_variables(world);

            let mut all_names: Vec<_> = model_vars;
            all_names.extend(model_inputs);
            all_names.extend(exp_vars.iter().cloned());
            all_names.sort();
            all_names.dedup();

            // Snapshot per-variable descriptions from the document index
            // up front so the row loop doesn't reborrow the registry per
            // checkbox.
            let var_desc: std::collections::HashMap<String, String> = {
                let registry = world.get_resource::<crate::ui::ModelicaDocumentRegistry>();
                let index_ref = registry
                    .and_then(|r| r.host(doc_id))
                    .map(|h| h.document().index());
                all_names
                    .iter()
                    .filter_map(|n| {
                        let entry = index_ref.and_then(|idx| idx.find_component_by_leaf(n))?;
                        if entry.description.is_empty() {
                            None
                        } else {
                            Some((n.clone(), entry.description.clone()))
                        }
                    })
                    .collect()
            };

            // Group by leading dotted segment for compactness.
            // Filtering happens before grouping so empty groups don't
            // render at all.
            let mut groups: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for name in all_names {
                if !filter_lower.is_empty()
                    && !name.to_ascii_lowercase().contains(&filter_lower)
                {
                    continue;
                }
                let head = name.split('.').next().unwrap_or(name.as_str()).to_string();
                groups.entry(head).or_default().push(name);
            }

            let mut toggles: Vec<(String, bool)> = Vec::new();
            for (group_name, names) in &groups {
                let picked_in_group = names
                    .iter()
                    .filter(|n| plotted.contains(*n) || picked_exp.contains(*n))
                    .count();
                let header = if picked_in_group > 0 {
                    format!("{} ({}/{})", group_name, picked_in_group, names.len())
                } else {
                    format!("{} ({})", group_name, names.len())
                };
                let default_open = !filter_lower.is_empty() || picked_in_group > 0;
                egui::CollapsingHeader::new(header)
                    .id_salt(format!("telem_var_group_{group_name}"))
                    .default_open(default_open)
                    .show(ui, |ui| {
                        for name in names {
                            let mut is_picked =
                                plotted.contains(name) || picked_exp.contains(name);
                            ui.horizontal(|ui| {
                                if ui.checkbox(&mut is_picked, "").changed() {
                                    toggles.push((name.clone(), is_picked));
                                }
                                let short = name
                                    .strip_prefix(&format!("{group_name}."))
                                    .unwrap_or(name);
                                let label =
                                    egui::Label::new(short).sense(egui::Sense::hover());
                                let resp = ui.add(label);
                                if let Some(desc) =
                                    var_desc.get(name).filter(|d| !d.trim().is_empty())
                                {
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
                        }
                    });
            }
            if groups.is_empty() {
                ui.weak("No variables match the filter.");
            }

            // Apply toggles after the loop — avoids reborrowing
            // resources mid-iteration. Each toggle writes to BOTH the
            // viz registry (live cosim) and ExperimentVisibility
            // (Fast Run) so the user picks once.
            for (name, on) in toggles {
                if let Some(mut reg) =
                    world.get_resource_mut::<lunco_viz::VisualizationRegistry>()
                {
                    set_signal_plotted(
                        &mut reg,
                        lunco_viz::SignalRef::new(entity, name.clone()),
                        on,
                    );
                }
                if let Some(mut states) = world
                    .get_resource_mut::<crate::ui::panels::experiments::PlotPanelStates>()
                {
                    states.set_var(active_plot, name, on);
                }
            }
            let _ = is_signal_plotted; // re-export available for future UIs
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

/// Render every top-level `parameter` / `constant` declaration on the
/// active class as an editable list. Complements
/// [`render_selected_components_inspector`] by surfacing parameters
/// declared *directly* on the root model — these have no canvas icon
/// and would otherwise be unreachable through the inspector.
///
/// Reads from the document's [`crate::index::ModelicaIndex`] (already
/// kept current by the op pipeline) — no AST walk per frame, no engine
/// lock, no shadow ECS state. Edits dispatch
/// `ModelicaOp::SetParameter { component, param: "", value }` — the
/// `""` sentinel routes the value into the component's primary
/// binding.
fn render_active_class_parameters(
    ui: &mut egui::Ui,
    world: &mut World,
    muted: egui::Color32,
) {
    use crate::document::ModelicaOp;
    use crate::index::Variability;
    use crate::ui::panels::canvas_diagram::{
        active_class_for_doc, active_doc_from_world, apply_ops_public,
    };

    let Some(doc_id) = active_doc_from_world(world) else { return };
    let Some(active) = active_class_for_doc(world, doc_id) else { return };

    // Snapshot rows from the index up front so we can release the
    // registry borrow before issuing apply_ops_public mutations.
    struct Row { name: String, value: String }
    let rows: Vec<Row> = {
        let Some(registry) = world.get_resource::<crate::ui::ModelicaDocumentRegistry>() else {
            return;
        };
        let Some(host) = registry.host(doc_id) else { return };
        let index = host.document().index();
        let Some(keys) = index.components_by_class.get(&active) else {
            return;
        };
        keys.iter()
            .filter_map(|k| index.components.get(k.0 as usize))
            .filter(|e| matches!(e.variability, Variability::Parameter | Variability::Constant))
            .map(|e| Row {
                name: e.name.clone(),
                value: e.binding.clone().unwrap_or_default(),
            })
            .collect()
    };
    if rows.is_empty() {
        return;
    }

    egui::CollapsingHeader::new(format!("⚙ Parameters ({})", rows.len()))
        .id_salt("active_class_parameters")
        .default_open(true)
        .show(ui, |ui| {
            // Two-pass: gather edits, apply after the immutable borrow
            // on `rows` is released.
            let mut edits: Vec<(String, String)> = Vec::new();
            for row in &rows {
                let mut buf = row.value.clone();
                ui.horizontal(|ui| {
                    ui.label(format!("{:14}", row.name));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut buf).desired_width(120.0),
                    );
                    let commit = (resp.lost_focus()
                        && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        || (resp.lost_focus() && buf != row.value);
                    if commit && buf != row.value {
                        edits.push((row.name.clone(), buf.clone()));
                    }
                });
            }
            if edits.is_empty() {
                let _ = muted;
                return;
            }
            let ops: Vec<ModelicaOp> = edits
                .into_iter()
                .map(|(component, value)| ModelicaOp::SetParameter {
                    class: active.clone(),
                    component,
                    param: String::new(),
                    value,
                })
                .collect();
            apply_ops_public(world, doc_id, ops);
        });
    ui.separator();
}
