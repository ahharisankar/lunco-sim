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
use egui_plot::{Legend, Line, Plot, PlotPoints};
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

        // Override editor for the currently active model. Rendered
        // collapsed by default; opens when the user wants to tweak
        // parameters before the next Fast Run.
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
                        ui.label(&row.name);
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

        ui.separator();
        self.render_plot_section(ui, world, &twin);
    }
}

impl ExperimentsPanel {
    /// Inline multi-series plot over the panel's checked experiments.
    /// Variable picker on the left; plot on the right. Each visible
    /// experiment contributes one curve per picked variable, color
    /// locked to the experiment's `color_hint`.
    fn render_plot_section(&self, ui: &mut egui::Ui, world: &mut World, twin: &TwinId) {
        // Snapshot relevant data so we can render without holding
        // resource borrows across egui calls.
        let (visible, picked_vars) = world
            .get_resource::<ExperimentVisibility>()
            .map(|v| (v.visible.clone(), v.picked_vars.clone()))
            .unwrap_or_default();

        // All variables across visible+done experiments — the picker's
        // candidate set.
        let mut all_vars: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        let mut series: Vec<PlotSeries> = Vec::new();
        if let Some(reg) = world.get_resource::<ExperimentRegistry>() {
            for exp in reg.list_for_twin(twin) {
                let Some(result) = &exp.result else { continue };
                for k in result.series.keys() {
                    all_vars.insert(k.clone());
                }
                if !visible.contains(&exp.id) {
                    continue;
                }
                for var in &picked_vars {
                    if let Some(values) = result.series.get(var) {
                        let pts: Vec<[f64; 2]> = result
                            .times
                            .iter()
                            .zip(values.iter())
                            .map(|(t, y)| [*t, *y])
                            .collect();
                        series.push(PlotSeries {
                            label: format!("{} · {}", exp.name, var),
                            color: palette_color(exp.color_hint),
                            points: pts,
                        });
                    }
                }
            }
        }

        ui.horizontal(|ui| {
            ui.weak("Variables:");
            if all_vars.is_empty() {
                ui.weak("(no completed experiments yet)");
                return;
            }
            // Compact toggleable list. For long var lists wrap in a
            // scroll area. Variables are dotted Modelica paths so they
            // can get long; wrap horizontally.
            let mut toggle_var: Option<String> = None;
            egui::ScrollArea::horizontal()
                .id_salt("var_picker_scroll")
                .show(ui, |ui| {
                    for var in &all_vars {
                        let mut on = picked_vars.contains(var);
                        if ui.checkbox(&mut on, var).changed() {
                            toggle_var = Some(var.clone());
                        }
                    }
                });
            if let Some(v) = toggle_var {
                if let Some(mut vis) = world.get_resource_mut::<ExperimentVisibility>() {
                    vis.toggle_var(v);
                }
            }
        });

        if series.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.weak("Tick experiments above and pick a variable to plot.");
            });
            return;
        }

        Plot::new("experiments_inline_plot")
            .legend(Legend::default())
            .height(220.0)
            .show(ui, |plot_ui| {
                for s in &series {
                    let (r, g, b) = s.color;
                    let line = Line::new(s.label.clone(), PlotPoints::from(s.points.clone()))
                        .color(egui::Color32::from_rgb(r, g, b));
                    plot_ui.line(line);
                }
            });
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
