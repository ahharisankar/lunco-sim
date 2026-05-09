//! Experiments panel — list of Fast Run experiments per twin.
//!
//! Status / spec: `docs/architecture/25-experiments.md`. v1 minimal:
//! - List each experiment in the registry (currently single "default" twin).
//! - Show name, bounds, status, duration, error.
//! - Plot-visibility checkbox (consumed by Graphs panel in Step 7).
//! - Cancel button on Running rows.
//! - Delete on terminal rows (context menu / button).
//!
//! Color picker, inline rename, click-to-load-draft are TODOs left
//! for the v1 polish pass.

use std::collections::BTreeMap;

use bevy::prelude::*;
use bevy_egui::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints, VLine};
use lunco_experiments::{
    ExperimentId, ExperimentRegistry, RunStatus, TwinId,
};
use lunco_workbench::{Panel, PanelId, PanelSlot};

pub const EXPERIMENTS_PANEL_ID: PanelId = PanelId("modelica_experiments");

/// Per-experiment plot visibility + selected variables. Decoupled
/// from the [`lunco_experiments::Experiment`] data so visibility is
/// a UI concern only and doesn't pollute the backend-agnostic crate.
#[derive(Resource, Default, Debug)]
pub struct ExperimentVisibility {
    pub visible: std::collections::HashSet<ExperimentId>,
    /// Variables ticked for plotting. Plotted once per visible
    /// experiment that has the variable.
    pub picked_vars: std::collections::BTreeSet<String>,
    /// Free-text filter for the variable picker. Case-insensitive
    /// substring match against the dotted variable path.
    pub var_filter: String,
    /// Current scrub time. `None` = pinned to the latest sample of
    /// the latest visible experiment (default; what the canvas
    /// overlay was showing before the scrubber landed). `Some(t)` =
    /// snap canvas + plot cursor to the run's sample closest to `t`.
    /// Set by clicking on the experiments plot; cleared via the
    /// "↻ Reset" button.
    pub scrub_time: Option<f64>,
}

impl ExperimentVisibility {
    pub fn is_visible(&self, id: ExperimentId) -> bool {
        self.visible.contains(&id)
    }
    pub fn toggle(&mut self, id: ExperimentId) {
        if !self.visible.insert(id) {
            self.visible.remove(&id);
        }
    }
    pub fn toggle_var(&mut self, var: String) {
        if !self.picked_vars.insert(var.clone()) {
            self.picked_vars.remove(&var);
        }
    }
}

pub struct ExperimentsPanel;

impl Panel for ExperimentsPanel {
    fn id(&self) -> PanelId {
        EXPERIMENTS_PANEL_ID
    }

    fn title(&self) -> String {
        "🧪 Experiments".into()
    }

    fn default_slot(&self) -> PanelSlot {
        PanelSlot::Bottom
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        // v1 single-twin scope. Multi-twin filter lands when the twin
        // browser plumbs an active TwinId through the workspace.
        let twin = TwinId("default".into());

        // Persistent Setup section — bounds + inputs editable inline,
        // ⏩ Run dispatches without re-opening the modal. Replaces the
        // "open dialog every time" friction.
        self.render_setup_section(ui, world);
        ui.separator();

        // Full override editor (non-input parameters). Stays
        // collapsed; only opened when needed.
        self.render_override_editor(ui, world);
        ui.separator();

        // Snapshot for rendering — avoids holding the registry borrow
        // across egui calls.
        let rows: Vec<Row> = match world.get_resource::<ExperimentRegistry>() {
            Some(reg) => reg
                .list_for_twin(&twin)
                .iter()
                .map(|e| Row {
                    id: e.id,
                    name: e.name.clone(),
                    bounds: format!(
                        "{:.2}..{:.2}{}",
                        e.bounds.t_start,
                        e.bounds.t_end,
                        match e.bounds.dt {
                            Some(d) => format!(", dt={d:.3}"),
                            None => " adaptive".into(),
                        }
                    ),
                    status: status_label(&e.status),
                    duration_ms: match &e.status {
                        RunStatus::Done { wall_time_ms } => Some(*wall_time_ms),
                        _ => None,
                    },
                    error: matches!(e.status, RunStatus::Failed { .. })
                        .then(|| match &e.status {
                            RunStatus::Failed { error, .. } => error.clone(),
                            _ => String::new(),
                        }),
                    is_terminal: e.status.is_terminal(),
                    color_hint: e.color_hint,
                    sample_count: e
                        .result
                        .as_ref()
                        .map(|r| r.times.len())
                        .unwrap_or(0),
                    var_count: e
                        .result
                        .as_ref()
                        .map(|r| r.series.len())
                        .unwrap_or(0),
                })
                .collect(),
            None => Vec::new(),
        };

        if rows.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(16.0);
                ui.weak("No experiments yet.");
                ui.weak("Press the ⏩ Fast button on a model to start one.");
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.weak(format!("{} experiment(s)", rows.len()));
        });
        ui.separator();

        let mut toggle: Option<ExperimentId> = None;
        let mut delete: Option<ExperimentId> = None;
        let mut cancel: Option<ExperimentId> = None;
        // Selected row → load its setup into the draft. Right-click
        // gives Re-run / Duplicate. Both work on terminal rows; for
        // running rows ⊘ Cancel is the only useful action.
        let mut load_into_draft: Option<ExperimentId> = None;
        let mut rerun: Option<ExperimentId> = None;
        let mut export_csv: Option<ExperimentId> = None;

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("experiments_table")
                .num_columns(7)
                .striped(true)
                .show(ui, |ui| {
                    ui.weak("👁");
                    ui.weak("Color");
                    ui.weak("Name");
                    ui.weak("Bounds");
                    ui.weak("Status");
                    ui.weak("Samples");
                    ui.weak("");
                    ui.end_row();

                    let visibility_snapshot: std::collections::HashSet<ExperimentId> = world
                        .get_resource::<ExperimentVisibility>()
                        .map(|v| v.visible.clone())
                        .unwrap_or_default();

                    for row in &rows {
                        let mut visible = visibility_snapshot.contains(&row.id);
                        if ui.checkbox(&mut visible, "").changed() {
                            toggle = Some(row.id);
                        }
                        let (r, g, b) = palette_color(row.color_hint);
                        ui.colored_label(
                            egui::Color32::from_rgb(r, g, b),
                            "■",
                        );
                        // Name as a clickable row entry — primary
                        // click loads the run's setup into the draft;
                        // right-click opens a context menu (Re-run /
                        // Duplicate / Delete). Hover hint nudges the
                        // user toward those actions.
                        let name_label = egui::Label::new(&row.name)
                            .sense(egui::Sense::click());
                        let name_resp = ui
                            .add(name_label)
                            .on_hover_text(
                                "Click: load this run's bounds / inputs / \
                                 overrides into the Setup draft. \
                                 Right-click: Re-run / Duplicate / Delete.",
                            );
                        if name_resp.clicked() && row.is_terminal {
                            load_into_draft = Some(row.id);
                        }
                        name_resp.context_menu(|ui| {
                            if row.is_terminal {
                                if ui.button("▶ Re-run with same setup").clicked() {
                                    rerun = Some(row.id);
                                    ui.close();
                                }
                                if ui.button("📋 Duplicate into Setup").clicked() {
                                    load_into_draft = Some(row.id);
                                    ui.close();
                                }
                                if ui
                                    .button("💾 Export CSV…")
                                    .on_hover_text(
                                        "Save this run's full trajectory \
                                         (time + every recorded variable) \
                                         to a CSV file.",
                                    )
                                    .clicked()
                                {
                                    export_csv = Some(row.id);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("✕ Delete").clicked() {
                                    delete = Some(row.id);
                                    ui.close();
                                }
                            } else if ui.button("⊘ Cancel run").clicked() {
                                cancel = Some(row.id);
                                ui.close();
                            }
                        });
                        ui.label(&row.bounds);
                        let status_widget = ui.label(&row.status);
                        if let Some(err) = &row.error {
                            status_widget.on_hover_text(err);
                        }
                        let sample_text = if row.var_count > 0 {
                            format!("{}×{}", row.sample_count, row.var_count)
                        } else if let Some(ms) = row.duration_ms {
                            format!("{} ms", ms)
                        } else {
                            String::new()
                        };
                        ui.label(sample_text);
                        if row.is_terminal {
                            if ui.small_button("✕").on_hover_text("Delete").clicked() {
                                delete = Some(row.id);
                            }
                        } else {
                            if ui
                                .small_button("⊘")
                                .on_hover_text("Cancel run")
                                .clicked()
                            {
                                cancel = Some(row.id);
                            }
                        }
                        ui.end_row();
                    }
                });
        });

        if let Some(id) = toggle {
            if let Some(mut v) = world.get_resource_mut::<ExperimentVisibility>() {
                v.toggle(id);
            }
        }
        if let Some(id) = delete {
            if let Some(mut reg) = world.get_resource_mut::<ExperimentRegistry>() {
                reg.delete(id);
            }
            if let Some(mut v) = world.get_resource_mut::<ExperimentVisibility>() {
                v.visible.remove(&id);
            }
        }
        if let Some(id) = cancel {
            // Best-effort cancel via the runner's RunHandle. The
            // PendingHandles drain system will see the resulting
            // RunUpdate::Cancelled and update registry status.
            if let Some(handles) = world
                .get_resource::<crate::experiments_runner::PendingHandles>()
            {
                for h in &handles.0 {
                    if h.run_id == id {
                        h.cancel();
                        break;
                    }
                }
            }
        }
        if let Some(id) = load_into_draft {
            load_run_into_draft(world, id);
        }
        if let Some(id) = export_csv {
            export_experiment_csv(world, id);
        }
        if let Some(id) = rerun {
            // Load setup, then dispatch a new Fast Run with it.
            // Resolving the originating doc keeps diagnostics routed
            // back to the right tab.
            load_run_into_draft(world, id);
            if let Some(doc) = world
                .get_resource::<crate::experiments_runner::ExperimentSources>()
                .and_then(|s| s.0.get(&id).copied())
                .or_else(|| {
                    world
                        .get_resource::<lunco_workbench::WorkspaceResource>()
                        .and_then(|ws| ws.active_document)
                })
            {
                world
                    .commands()
                    .trigger(crate::ui::commands::FastRunActiveModel { doc });
            }
        }

        // Plot + variable picker now live in the Graphs panel — this
        // panel is the run *list* / comparison-source. See the Source
        // toggle in panels::graphs.
    }
}

impl ExperimentsPanel {
    /// Persistent Setup section at the top of the Experiments panel.
    /// Compact bounds + inputs + Run button. Edits persist into the
    /// per-`ModelRef` draft; the toolbar's ⏩ Fast button reads the
    /// same draft, so changes here are visible there immediately.
    fn render_setup_section(&self, ui: &mut egui::Ui, world: &mut World) {
        // Resolve active doc + model class.
        let Some(doc) = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document)
        else {
            return;
        };
        let (model_name, source) = match world
            .get_resource::<crate::ui::state::ModelicaDocumentRegistry>()
            .and_then(|r| r.host(doc))
        {
            Some(h) => {
                let document = h.document();
                let drilled = world
                    .get_resource::<crate::ui::panels::model_view::ModelTabs>()
                    .and_then(|t| t.drilled_class_for_doc(doc));
                let first_non_pkg = document
                    .index()
                    .classes
                    .values()
                    .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                    .map(|c| c.name.clone());
                let class = drilled.or(first_non_pkg);
                match class {
                    Some(c) => (c, document.source().to_string()),
                    None => return,
                }
            }
            None => return,
        };
        let model_ref = lunco_experiments::ModelRef(model_name.clone());

        // Snapshot draft + runner defaults for prefill.
        let draft_bounds = world
            .get_resource::<crate::experiments_runner::ExperimentDrafts>()
            .and_then(|d| d.get(&model_ref).and_then(|dr| dr.bounds_override.clone()));
        let mut bounds = draft_bounds.unwrap_or_else(|| {
            world
                .get_resource::<crate::ModelicaRunnerResource>()
                .and_then(|r| {
                    use lunco_experiments::ExperimentRunner;
                    r.0.default_bounds(&model_ref)
                })
                .unwrap_or(lunco_experiments::RunBounds {
                    t_start: 0.0,
                    t_end: 1.0,
                    dt: None,
                    tolerance: None,
                    solver: None,
                })
        });
        let mut bounds_changed = false;

        let detected_inputs =
            crate::experiments_runner::detect_top_level_inputs(&source);
        let prefilled_inputs: BTreeMap<lunco_experiments::ParamPath, lunco_experiments::ParamValue> =
            world
                .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                .and_then(|d| d.get(&model_ref).map(|dr| dr.inputs.clone()))
                .unwrap_or_default();
        // Maintain editable text per input row across frames via a
        // local scratch in the panel — simpler than yet another
        // resource. Reset when model changes.
        let mut input_edits: Vec<(String, String, String)> = detected_inputs
            .iter()
            .map(|d| {
                let txt = prefilled_inputs
                    .get(&lunco_experiments::ParamPath(d.name.clone()))
                    .map(|v| match v {
                        lunco_experiments::ParamValue::Real(x) => format!("{x}"),
                        lunco_experiments::ParamValue::Int(x) => format!("{x}"),
                        lunco_experiments::ParamValue::Bool(b) => {
                            if *b { "true".into() } else { "false".into() }
                        }
                        lunco_experiments::ParamValue::String(s) => s.clone(),
                        lunco_experiments::ParamValue::Enum(s) => s.clone(),
                        lunco_experiments::ParamValue::RealArray(_) => "(array)".into(),
                    })
                    .unwrap_or_default();
                (d.name.clone(), d.type_name.clone(), txt)
            })
            .collect();
        let mut inputs_changed = false;
        let mut run_clicked = false;

        let runner_busy = world
            .get_resource::<crate::ModelicaRunnerResource>()
            .map(|r| r.0.is_busy())
            .unwrap_or(false);

        // Annotation-default reference for "is this what the model
        // says?" tagging next to the bounds inputs.
        let annotation_defaults = world
            .get_resource::<crate::ModelicaRunnerResource>()
            .and_then(|r| {
                use lunco_experiments::ExperimentRunner;
                r.0.default_bounds(&model_ref)
            });
        let from_annotation = annotation_defaults.is_some();

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!("Setup — {}", model_name)).strong());
            if from_annotation {
                ui.weak("· bounds default from experiment(...) annotation");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if runner_busy { "⏩ Running…" } else { "⏩ Run" };
                let valid = bounds.t_end > bounds.t_start;
                if ui.add_enabled(valid && !runner_busy, egui::Button::new(label)).clicked() {
                    run_clicked = true;
                }
            });
        });

        // Compact bounds row.
        ui.horizontal(|ui| {
            ui.label("t:");
            if ui.add(egui::DragValue::new(&mut bounds.t_start).speed(0.1)).changed() {
                bounds_changed = true;
            }
            ui.label("→");
            if ui.add(egui::DragValue::new(&mut bounds.t_end).speed(0.1)).changed() {
                bounds_changed = true;
            }
            ui.label("s");
            ui.separator();
            let mut adaptive = bounds.dt.is_none();
            let mut dt_v = bounds.dt.unwrap_or(0.01);
            if ui.checkbox(&mut adaptive, "adaptive dt").changed() {
                bounds.dt = if adaptive { None } else { Some(0.01) };
                bounds_changed = true;
            }
            if !adaptive
                && ui
                    .add(
                        egui::DragValue::new(&mut dt_v)
                            .speed(0.001)
                            .range(1e-6..=10.0),
                    )
                    .changed()
            {
                bounds.dt = Some(dt_v);
                bounds_changed = true;
            }
        });

        // Inputs row(s). Wrap horizontally — a model with many
        // inputs scrolls instead of growing vertically.
        if !input_edits.is_empty() {
            egui::ScrollArea::horizontal()
                .id_salt("setup_inputs_scroll")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.weak("Inputs:");
                        for (name, _ty, value_text) in input_edits.iter_mut() {
                            ui.label(name.as_str());
                            let resp = ui.add(
                                egui::TextEdit::singleline(value_text)
                                    .desired_width(70.0),
                            );
                            if resp.changed() || resp.lost_focus() {
                                inputs_changed = true;
                            }
                        }
                    });
                });
        }

        // Persist edits.
        if bounds_changed {
            if let Some(mut drafts) = world
                .get_resource_mut::<crate::experiments_runner::ExperimentDrafts>()
            {
                drafts.entry(model_ref.clone()).bounds_override = Some(bounds);
            }
        }
        if inputs_changed {
            // Build a new BTreeMap from edited text.
            let mut map: BTreeMap<lunco_experiments::ParamPath, lunco_experiments::ParamValue> =
                BTreeMap::new();
            for (name, ty, text) in input_edits.iter() {
                let s = text.trim();
                if s.is_empty() {
                    continue;
                }
                let v = match ty.as_str() {
                    "Real" => s.parse::<f64>().ok().map(lunco_experiments::ParamValue::Real),
                    "Integer" | "Int" => s.parse::<i64>().ok().map(lunco_experiments::ParamValue::Int),
                    "Boolean" | "Bool" => match s {
                        "true" => Some(lunco_experiments::ParamValue::Bool(true)),
                        "false" => Some(lunco_experiments::ParamValue::Bool(false)),
                        _ => None,
                    },
                    _ => s.parse::<f64>().ok().map(lunco_experiments::ParamValue::Real),
                };
                if let Some(v) = v {
                    map.insert(lunco_experiments::ParamPath(name.clone()), v);
                }
            }
            if let Some(mut drafts) = world
                .get_resource_mut::<crate::experiments_runner::ExperimentDrafts>()
            {
                drafts.entry(model_ref).inputs = map;
            }
        }
        if run_clicked {
            // Skip the modal — Setup is already filled in.
            world
                .commands()
                .trigger(crate::ui::commands::FastRunActiveModel { doc });
        }
    }

    /// Override + bounds editor for the currently active document's
    /// top-level model. Detects literal `parameter` declarations in
    /// the source and shows them as an editable table; non-literal
    /// params appear greyed with a tooltip.
    fn render_override_editor(&self, ui: &mut egui::Ui, world: &mut World) {
        let Some(doc) = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document)
        else {
            return;
        };
        let registry = match world.get_resource::<crate::ui::state::ModelicaDocumentRegistry>() {
            Some(r) => r,
            None => return,
        };
        let host = match registry.host(doc) {
            Some(h) => h,
            None => return,
        };
        let document = host.document();
        let source = document.source().to_string();

        // Resolve the model class — first non-package top-level class.
        let model_name = document
            .strict_ast()
            .and_then(|ast| {
                ast.classes
                    .iter()
                    .find(|(_, c)| {
                        !matches!(
                            c.class_type,
                            rumoca_session::parsing::ast::ClassType::Package
                        )
                    })
                    .map(|(n, _)| n.clone())
            })
            .unwrap_or_default();
        if model_name.is_empty() {
            return;
        }
        let model_ref = lunco_experiments::ModelRef(model_name.clone());

        let detected =
            crate::experiments_runner::detect_top_level_literal_parameters(&source);
        if detected.is_empty() {
            return;
        }

        egui::CollapsingHeader::new(format!("⚙ Overrides + Bounds — {}", model_name))
            .default_open(false)
            .show(ui, |ui| {
                use lunco_experiments::{ParamPath, ParamValue};

                // Bounds editor
                let mut bounds = world
                    .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                    .and_then(|d| {
                        d.get(&model_ref)
                            .and_then(|dr| dr.bounds_override.clone())
                    })
                    .unwrap_or(lunco_experiments::RunBounds {
                        t_start: 0.0,
                        t_end: 1.0,
                        dt: None,
                        tolerance: None,
                        solver: None,
                    });
                let mut bounds_changed = false;
                ui.horizontal(|ui| {
                    ui.label("t_start");
                    if ui.add(egui::DragValue::new(&mut bounds.t_start).speed(0.1)).changed() {
                        bounds_changed = true;
                    }
                    ui.label("t_end");
                    if ui.add(egui::DragValue::new(&mut bounds.t_end).speed(0.1)).changed() {
                        bounds_changed = true;
                    }
                    let mut dt_v = bounds.dt.unwrap_or(0.0);
                    let mut adaptive = bounds.dt.is_none();
                    if ui.checkbox(&mut adaptive, "adaptive dt").changed() {
                        bounds.dt = if adaptive { None } else { Some(0.01) };
                        bounds_changed = true;
                    }
                    if !adaptive
                        && ui
                            .add(
                                egui::DragValue::new(&mut dt_v)
                                    .speed(0.001)
                                    .range(1e-6..=10.0),
                            )
                            .changed()
                    {
                        bounds.dt = Some(dt_v);
                        bounds_changed = true;
                    }
                });
                if bounds_changed {
                    if let Some(mut drafts) = world
                        .get_resource_mut::<crate::experiments_runner::ExperimentDrafts>()
                    {
                        drafts.entry(model_ref.clone()).bounds_override = Some(bounds);
                    }
                }

                ui.separator();

                // Parameter overrides
                let current_overrides: BTreeMap<ParamPath, ParamValue> = world
                    .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                    .and_then(|d| d.get(&model_ref).map(|dr| dr.overrides.clone()))
                    .unwrap_or_default();

                let mut updates: Vec<(ParamPath, Option<ParamValue>)> = Vec::new();

                egui::Grid::new("override_grid")
                    .num_columns(4)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.weak("Type");
                        ui.weak("Name");
                        ui.weak("Default");
                        ui.weak("Override");
                        ui.end_row();

                        for p in &detected {
                            ui.label(&p.type_name);
                            ui.label(&p.name);
                            ui.label(p.default_literal.as_deref().unwrap_or("—"));
                            let path = ParamPath(p.name.clone());
                            if !p.supportable {
                                ui.add_enabled(
                                    false,
                                    egui::TextEdit::singleline(&mut String::from("—"))
                                        .desired_width(80.0),
                                )
                                .on_hover_text(
                                    p.reason
                                        .clone()
                                        .unwrap_or_else(|| "unsupported".into()),
                                );
                            } else {
                                let existing = current_overrides.get(&path).cloned();
                                let mut text = match &existing {
                                    Some(ParamValue::Real(x)) => format!("{x}"),
                                    Some(ParamValue::Int(x)) => format!("{x}"),
                                    Some(ParamValue::Bool(b)) => {
                                        if *b { "true".into() } else { "false".into() }
                                    }
                                    Some(ParamValue::String(s)) => s.clone(),
                                    Some(ParamValue::Enum(s)) => s.clone(),
                                    Some(ParamValue::RealArray(_)) => "(array)".into(),
                                    None => p.default_literal.clone().unwrap_or_default(),
                                };
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut text).desired_width(80.0),
                                );
                                if resp.lost_focus()
                                    || resp.ctx.input(|i| i.key_pressed(egui::Key::Enter))
                                        && resp.has_focus()
                                {
                                    if let Some(v) = parse_override(&p.type_name, &text) {
                                        updates.push((path.clone(), Some(v)));
                                    } else if text.trim().is_empty() {
                                        updates.push((path.clone(), None));
                                    }
                                }
                                if existing.is_some() {
                                    if ui
                                        .small_button("×")
                                        .on_hover_text("Clear override")
                                        .clicked()
                                    {
                                        updates.push((path, None));
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });

                if !updates.is_empty() {
                    if let Some(mut drafts) = world
                        .get_resource_mut::<crate::experiments_runner::ExperimentDrafts>()
                    {
                        let entry = drafts.entry(model_ref);
                        for (path, v) in updates {
                            match v {
                                Some(value) => {
                                    entry.overrides.insert(path, value);
                                }
                                None => {
                                    entry.overrides.remove(&path);
                                }
                            }
                        }
                    }
                }
            });
    }
}

fn parse_override(type_name: &str, text: &str) -> Option<lunco_experiments::ParamValue> {
    use lunco_experiments::ParamValue;
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    match type_name {
        "Real" => s.parse::<f64>().ok().map(ParamValue::Real),
        "Integer" | "Int" => s.parse::<i64>().ok().map(ParamValue::Int),
        "Boolean" | "Bool" => match s {
            "true" => Some(ParamValue::Bool(true)),
            "false" => Some(ParamValue::Bool(false)),
            _ => None,
        },
        "String" => {
            if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                Some(ParamValue::String(s[1..s.len() - 1].to_string()))
            } else {
                Some(ParamValue::String(s.to_string()))
            }
        }
        _ => {
            // Best-effort fallback: if it parses as a number, keep it
            // as Real. Otherwise treat as Enum literal name.
            if let Ok(x) = s.parse::<f64>() {
                Some(ParamValue::Real(x))
            } else {
                Some(ParamValue::Enum(s.to_string()))
            }
        }
    }
}

struct PlotSeries {
    label: String,
    color: (u8, u8, u8),
    points: Vec<[f64; 2]>,
}

/// Render the experiments multi-series plot. Picker lives in
/// Telemetry now; this just collects whatever variables Telemetry
/// has ticked + every visible experiment, builds series, and fills
/// the available space. v1 single-twin scope.
///
/// Variable units are pulled from the active doc's per-component
/// index (`modifications.get("unit")`) and surfaced two ways:
/// - Legend: `Run 1 · engine.thrust [N]`.
/// - Y-axis label: shows the unit when every visible variable shares
///   one; otherwise blank (mixed-unit plots happen often when users
///   tick variables across components).
pub fn render_experiments_plot(ui: &mut egui::Ui, world: &mut World) -> ExpPlotSummary {
    let twin = TwinId("default".into());

    let (visible, picked_vars) = world
        .get_resource::<ExperimentVisibility>()
        .map(|v| (v.visible.clone(), v.picked_vars.clone()))
        .unwrap_or_default();

    // Build var -> unit map from the active doc index.
    let units: std::collections::HashMap<String, String> = active_doc_units(world);

    let mut series: Vec<PlotSeries> = Vec::new();
    let mut total_runs = 0usize;
    let mut visible_runs = 0usize;
    let mut shared_unit: Option<String> = None;
    let mut shared_unit_init = false;
    if let Some(reg) = world.get_resource::<ExperimentRegistry>() {
        for exp in reg.list_for_twin(&twin) {
            total_runs += 1;
            let Some(result) = &exp.result else { continue };
            if !visible.contains(&exp.id) {
                continue;
            }
            visible_runs += 1;
            for var in &picked_vars {
                if let Some(values) = result.series.get(var) {
                    let unit = units.get(var).cloned();
                    // Track shared-unit-across-series for the y-axis
                    // label; flip to None on first mismatch.
                    if !shared_unit_init {
                        shared_unit = unit.clone();
                        shared_unit_init = true;
                    } else if shared_unit != unit {
                        shared_unit = None;
                    }
                    let label = match &unit {
                        Some(u) if !u.is_empty() => {
                            format!("{} · {} [{}]", exp.name, var, u)
                        }
                        _ => format!("{} · {}", exp.name, var),
                    };
                    let pts: Vec<[f64; 2]> = result
                        .times
                        .iter()
                        .zip(values.iter())
                        .map(|(t, y)| [*t, *y])
                        .collect();
                    series.push(PlotSeries {
                        label,
                        color: palette_color(exp.color_hint),
                        points: pts,
                    });
                }
            }
        }
    }

    let scrub_time = world
        .get_resource::<ExperimentVisibility>()
        .and_then(|v| v.scrub_time);

    let mut new_scrub: Option<Option<f64>> = None;

    if !series.is_empty() {
        // Compact toolbar above the chart: scrub-time readout + reset.
        let mut reset_clicked = false;
        ui.horizontal(|ui| {
            match scrub_time {
                Some(t) => {
                    ui.label(
                        egui::RichText::new(format!("⏱ scrub: t = {t:.3} s"))
                            .size(11.0)
                            .monospace(),
                    );
                    if ui
                        .small_button("↻ reset")
                        .on_hover_text("Drop the scrub cursor — canvas overlay snaps back to the run's final time")
                        .clicked()
                    {
                        reset_clicked = true;
                    }
                }
                None => {
                    ui.weak("Click the plot to scrub time — canvas overlays follow the cursor.");
                }
            }
        });
        if reset_clicked {
            new_scrub = Some(None);
        }

        let mut plot = Plot::new("graphs_experiments_plot")
            .legend(Legend::default())
            .x_axis_label("t [s]")
            // Don't let the dragger eat clicks — we want clicks to set
            // the scrub cursor instead of pan/zoom. Box-zoom stays on
            // the modifier defaults; double-click still resets bounds.
            .allow_drag(false);
        if let Some(u) = shared_unit.as_ref().filter(|u| !u.is_empty()) {
            plot = plot.y_axis_label(format!("[{u}]"));
        }
        let captured_x: std::cell::Cell<Option<f64>> = std::cell::Cell::new(None);
        plot.show(ui, |plot_ui| {
            for s in &series {
                let (r, g, b) = s.color;
                let line = Line::new(s.label.clone(), PlotPoints::from(s.points.clone()))
                    .color(egui::Color32::from_rgb(r, g, b));
                plot_ui.line(line);
            }
            if let Some(t) = scrub_time {
                plot_ui.vline(
                    VLine::new("scrub", t)
                        .color(egui::Color32::from_rgb(220, 220, 100))
                        .width(1.5),
                );
            }
            // Click anywhere on the chart sets the scrub time. Drag
            // is disabled (allow_drag=false above) so clicks aren't
            // ambiguous with pan.
            if plot_ui.response().clicked() {
                if let Some(p) = plot_ui.pointer_coordinate() {
                    captured_x.set(Some(p.x));
                }
            }
        });
        if let Some(x) = captured_x.get() {
            new_scrub = Some(Some(x));
        }
    }

    if let Some(s) = new_scrub {
        if let Some(mut vis) = world.get_resource_mut::<ExperimentVisibility>() {
            vis.scrub_time = s;
        }
    }
    ExpPlotSummary {
        total_runs,
        visible_runs,
        series_drawn: series.len(),
        picked_vars: picked_vars.len(),
    }
}

/// Write a completed experiment's full trajectory to a user-picked
/// CSV file. Format: header `time,<var1>,<var2>,…` followed by one
/// row per sample. All variables share the run's `times` vector
/// already, so no resampling is needed (unlike the live-cosim CSV
/// export in the Graphs panel which has to merge per-signal histories).
///
/// Routes through `lunco_storage::FileStorage` so the same call site
/// will work when an OPFS / browser-download backend lands for wasm.
/// Cancelling the picker is a silent no-op; errors land in Console.
fn export_experiment_csv(world: &mut World, id: ExperimentId) {
    use lunco_storage::Storage as _;

    let (file_stem, csv_text) = {
        let registry = match world.get_resource::<ExperimentRegistry>() {
            Some(r) => r,
            None => return,
        };
        let Some(exp) = registry.get(id) else { return };
        let Some(result) = &exp.result else {
            if let Some(mut console) =
                world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
            {
                console.error(
                    "CSV export: experiment has no result yet (still running or failed)",
                );
            }
            return;
        };
        let mut text = String::new();
        // Header row.
        text.push_str("time");
        let mut var_order: Vec<&String> = result.series.keys().collect();
        var_order.sort();
        for v in &var_order {
            text.push(',');
            // Quote names that contain commas / quotes; Modelica
            // dotted paths normally don't, but be defensive.
            push_csv_field(&mut text, v);
        }
        text.push('\n');
        // Data rows.
        for (i, t) in result.times.iter().enumerate() {
            text.push_str(&format!("{t}"));
            for v in &var_order {
                text.push(',');
                let val = result.series.get(*v).and_then(|col| col.get(i));
                match val {
                    Some(x) if x.is_finite() => text.push_str(&format!("{x}")),
                    _ => {} // empty cell for NaN / out-of-range
                }
            }
            text.push('\n');
        }
        // Filename suggestion: model+run-id-suffix sanitised.
        let safe_name: String = exp
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        (safe_name, text)
    };

    let storage = lunco_storage::FileStorage::new();
    let hint = lunco_storage::SaveHint {
        suggested_name: Some(format!("{file_stem}.csv")),
        start_dir: None,
        filters: vec![lunco_storage::OpenFilter::new("CSV", &["csv"])],
    };
    let handle = match storage.pick_save(&hint) {
        Ok(Some(h)) => h,
        Ok(None) => return,
        Err(e) => {
            if let Some(mut console) =
                world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
            {
                console.error(format!("CSV export: picker failed: {e}"));
            }
            return;
        }
    };
    if let Err(e) = storage.write(&handle, csv_text.as_bytes()) {
        if let Some(mut console) =
            world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
        {
            console.error(format!("CSV export: write failed: {e}"));
        }
    } else if let Some(mut console) =
        world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
    {
        console.info(format!("✓ Exported experiment to {file_stem}.csv"));
    }
}

fn push_csv_field(out: &mut String, s: &str) {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        out.push('"');
        for c in s.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// Copy a completed experiment's bounds + inputs + overrides into
/// the per-`ModelRef` draft. The toolbar's bounds readout, the
/// inline Setup section, and the Setup modal all read from that
/// draft, so a row click is enough to "fork" a previous run as the
/// next setup. Pure World mutation; no event dispatched.
fn load_run_into_draft(world: &mut World, id: ExperimentId) {
    let snapshot = {
        let registry = match world.get_resource::<ExperimentRegistry>() {
            Some(r) => r,
            None => return,
        };
        registry.get(id).map(|e| (
                e.model_ref.clone(),
                e.bounds.clone(),
                e.inputs.clone(),
                e.overrides.clone(),
            ))
    };
    let Some((model_ref, bounds, inputs, overrides)) = snapshot else {
        return;
    };
    if let Some(mut drafts) = world
        .get_resource_mut::<crate::experiments_runner::ExperimentDrafts>()
    {
        let entry = drafts.entry(model_ref);
        entry.bounds_override = Some(bounds);
        entry.inputs = inputs;
        entry.overrides = overrides;
    }
}

/// Build a `var_path -> unit` map for whatever the picker has
/// selected, by querying the active document's component index.
/// Walks `picked_vars` directly so the cost stays O(picks) instead
/// of O(all-components-in-the-model).
///
/// Uses [`ModelicaIndex::find_component_by_leaf`] so dotted paths
/// like `engine.thrust` resolve to a component declared somewhere
/// in the model with leaf name `thrust`. First match wins on
/// collisions across classes — same trade-off the rest of the UI
/// already makes.
fn active_doc_units(world: &World) -> std::collections::HashMap<String, String> {
    let mut out: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let Some(doc) = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
    else {
        return out;
    };
    let Some(registry) = world.get_resource::<crate::ui::state::ModelicaDocumentRegistry>()
    else {
        return out;
    };
    let Some(host) = registry.host(doc) else {
        return out;
    };
    let Some(picked) = world
        .get_resource::<ExperimentVisibility>()
        .map(|v| v.picked_vars.clone())
    else {
        return out;
    };
    let document = host.document();
    let index = document.index();
    for var in &picked {
        if let Some(entry) = index.find_component_by_leaf(var) {
            if let Some(unit) = entry.modifications.get("unit") {
                if !unit.is_empty() {
                    out.insert(var.clone(), unit.clone());
                }
            }
        }
    }
    out
}

/// Aggregate counters returned by [`render_experiments_plot`] so the
/// Graphs panel can fold them into its single header line instead of
/// rendering its own status text.
pub struct ExpPlotSummary {
    pub total_runs: usize,
    pub visible_runs: usize,
    pub series_drawn: usize,
    pub picked_vars: usize,
}

/// Compute an [`ExpPlotSummary`] without rendering. Lets the Graphs
/// panel show counts in its top header row before drawing the plot.
pub fn experiments_plot_summary(world: &World) -> ExpPlotSummary {
    let twin = TwinId("default".into());
    let (visible, picked_vars) = world
        .get_resource::<ExperimentVisibility>()
        .map(|v| (v.visible.clone(), v.picked_vars.clone()))
        .unwrap_or_default();
    let mut total_runs = 0usize;
    let mut visible_runs = 0usize;
    let mut series_drawn = 0usize;
    if let Some(reg) = world.get_resource::<ExperimentRegistry>() {
        for exp in reg.list_for_twin(&twin) {
            total_runs += 1;
            let Some(result) = &exp.result else { continue };
            if !visible.contains(&exp.id) {
                continue;
            }
            visible_runs += 1;
            for var in &picked_vars {
                if result.series.contains_key(var) {
                    series_drawn += 1;
                }
            }
        }
    }
    ExpPlotSummary {
        total_runs,
        visible_runs,
        series_drawn,
        picked_vars: picked_vars.len(),
    }
}

/// Collect every variable name across all completed experiments for
/// the active twin. Used by the Telemetry panel to surface
/// experiment-only variables alongside live cosim signals.
pub fn all_experiment_variables(world: &World) -> std::collections::BTreeSet<String> {
    let twin = TwinId("default".into());
    let mut out: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(reg) = world.get_resource::<ExperimentRegistry>() {
        for exp in reg.list_for_twin(&twin) {
            if let Some(result) = &exp.result {
                for k in result.series.keys() {
                    out.insert(k.clone());
                }
            }
        }
    }
    out
}

struct Row {
    id: ExperimentId,
    name: String,
    bounds: String,
    status: String,
    duration_ms: Option<u64>,
    error: Option<String>,
    is_terminal: bool,
    color_hint: u8,
    sample_count: usize,
    var_count: usize,
}

fn status_label(s: &RunStatus) -> String {
    match s {
        RunStatus::Pending => "⌛ Pending".into(),
        RunStatus::Running { t_current } => format!("▶ {t_current:.2}s"),
        RunStatus::Done { wall_time_ms } => format!("✓ Done ({wall_time_ms} ms)"),
        RunStatus::Failed { .. } => "⚠ Failed".into(),
        RunStatus::Cancelled => "⊘ Cancelled".into(),
    }
}

/// Stable color palette indexed by `Experiment.color_hint`. Keep
/// in sync with the Graphs panel's per-series color (Step 7).
pub fn palette_color(idx: u8) -> (u8, u8, u8) {
    // 8-color qualitative palette; cycles via modulo so the registry
    // cap (20) doesn't matter for color reuse.
    const PALETTE: &[(u8, u8, u8)] = &[
        (66, 133, 244),  // blue
        (219, 68, 55),   // red
        (244, 180, 0),   // amber
        (15, 157, 88),   // green
        (171, 71, 188),  // purple
        (255, 112, 67),  // orange
        (38, 166, 154),  // teal
        (236, 64, 122),  // pink
    ];
    PALETTE[idx as usize % PALETTE.len()]
}
