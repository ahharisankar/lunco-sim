//! Modelica plot panel — multi-instance host for time-series plots.
//!
//! Each tab is a `ModelicaPlotPanel` instance keyed by `VizId`. The
//! historical singleton "Graphs" tab is now just the *first* instance,
//! auto-spawned at startup with `VizId = DEFAULT_MODELICA_GRAPH` and
//! the title "Graphs". Telemetry checkboxes still bind their signals
//! to that default config; users can open additional plots via
//! `NewPlotPanel` (the `➕` button) and each gets its own
//! `VisualizationConfig` with independent live-signal bindings.
//!
//! Experiments overlay state (`ExperimentVisibility`) is currently
//! shared across all plot instances — picked variables show up in
//! every plot's experiments section. Per-panel experiment state is
//! a follow-up; the live-signal split (the more impactful one) is in
//! place.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_workbench::{InstancePanel, PanelId, PanelSlot};
use lunco_viz::{
    export_signals_to_csv, kinds::line_plot::LinePlot, view::Panel2DCtx, viz::Visualization,
    viz::VizId, SignalRegistry, VisualizationRegistry, VizFitRequests,
};

use crate::ui::viz::{ensure_default_modelica_graph, DEFAULT_MODELICA_GRAPH};

/// Multi-instance kind id. Each instance is a `VizId.0`.
pub const MODELICA_PLOT_KIND: PanelId = PanelId("modelica_plot");

#[derive(Default)]
pub struct ModelicaPlotPanel;

impl InstancePanel for ModelicaPlotPanel {
    fn kind(&self) -> PanelId { MODELICA_PLOT_KIND }
    fn default_slot(&self) -> PanelSlot { PanelSlot::Bottom }

    fn title(&self, world: &World, instance: u64) -> String {
        let id = VizId(instance);
        // The default plot keeps the historical "Graphs" name. Other
        // instances use whatever title was set on creation, falling
        // back to "Plot #N" via the registry config.
        if id == DEFAULT_MODELICA_GRAPH {
            return "📈 Graphs".into();
        }
        world
            .get_resource::<VisualizationRegistry>()
            .and_then(|r| r.get(id))
            .map(|cfg| format!("📈 {}", cfg.title))
            .unwrap_or_else(|| format!("📈 Plot #{instance}"))
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World, instance: u64) {
        render_modelica_plot(ui, world, VizId(instance));
    }
}

/// Render body shared by every Modelica plot instance.
///
/// Reads the per-VizId `VisualizationConfig` (live-signal bindings),
/// renders the polished toolbar (live-summary / Fit / CSV / new-plot),
/// then dispatches to the experiments overlay + LinePlot kind.
fn render_modelica_plot(ui: &mut egui::Ui, world: &mut World, viz_id: VizId) {
    // Mark this plot as the active one for global readers (canvas
    // overlay, telemetry, runner auto-pick). Most-recently-rendered
    // wins; acceptable until per-plot focus tracking lands.
    if let Some(mut active) =
        world.get_resource_mut::<crate::ui::panels::experiments::ActivePlot>()
    {
        active.0 = Some(viz_id);
    }
    let muted = world
        .get_resource::<lunco_theme::Theme>()
        .map(|t| t.tokens.text_subdued)
        .unwrap_or(egui::Color32::DARK_GRAY);

    // Bootstrap the registry entry for the default graph the first
    // time the panel renders. Other VizIds were created by
    // `NewPlotPanel` and already exist; this branch is a no-op for
    // them.
    let (bound_count, time_min, time_max, sample_total) = {
        let Some(mut registry) = world.get_resource_mut::<VisualizationRegistry>() else {
            ui.label("lunco-viz not installed.");
            return;
        };
        let cfg_opt = if viz_id == DEFAULT_MODELICA_GRAPH {
            Some(ensure_default_modelica_graph(&mut registry).clone())
        } else {
            registry.get(viz_id).cloned()
        };
        let Some(cfg) = cfg_opt else {
            drop(registry);
            ui.label(format!("Plot #{} not found.", viz_id.0));
            return;
        };
        let count = cfg.inputs.len();
        let sources: Vec<_> = cfg.inputs.iter().map(|b| b.source.clone()).collect();
        drop(registry);

        let (mut t_min, mut t_max, mut total) = (f64::INFINITY, f64::NEG_INFINITY, 0usize);
        if let Some(sigs) = world.get_resource::<SignalRegistry>() {
            for src in &sources {
                if let Some(hist) = sigs.scalar_history(src) {
                    if let (Some(first), Some(last)) =
                        (hist.samples.front(), hist.samples.back())
                    {
                        t_min = t_min.min(first.time);
                        t_max = t_max.max(last.time);
                    }
                    total += hist.len();
                }
            }
        }
        (count, t_min, t_max, total)
    };

    let mut fit_clicked = false;
    let mut export_csv_clicked = false;
    // Per-plot experiment overlay: each tab has its own picked-vars
    // and scrub cursor, so every plot can render the experiments
    // overlay independently.
    let exp_summary =
        crate::ui::panels::experiments::experiments_plot_summary(world, viz_id);
    let has_live = bound_count > 0;
    let has_exp = exp_summary.total_runs > 0;

    let show_top_header = has_live || (!has_live && !has_exp);
    if show_top_header {
        ui.horizontal(|ui| {
            if has_live {
                ui.label(
                    egui::RichText::new(format!("Live: {bound_count} var"))
                        .size(11.0)
                        .color(muted),
                );
                if time_min.is_finite() && time_max.is_finite() {
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "t: {time_min:.2}→{time_max:.2}s  ({sample_total} samples)"
                        ))
                        .size(11.0)
                        .color(muted),
                    );
                }
            } else {
                ui.label(
                    egui::RichText::new(
                        "No data yet — pick variables in Telemetry and run a model.",
                    )
                    .size(11.0)
                    .color(muted),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let new_plot = ui
                    .small_button("➕")
                    .on_hover_text("New plot panel — opens a fresh tab.");
                if new_plot.clicked() {
                    world
                        .commands()
                        .trigger(crate::ui::commands::NewPlotPanel::default());
                }
                if has_live {
                    let fit = ui
                        .small_button("📐")
                        .on_hover_text("Auto-fit axes to current data.");
                    if fit.clicked() {
                        fit_clicked = true;
                    }
                    let csv = ui
                        .small_button("💾 CSV")
                        .on_hover_text("Export live signal histories to CSV.");
                    if csv.clicked() {
                        export_csv_clicked = true;
                    }
                }
            });
        });
        ui.separator();
    }
    if fit_clicked {
        if let Some(mut requests) = world.get_resource_mut::<VizFitRequests>() {
            requests.request(viz_id);
        }
    }
    if export_csv_clicked {
        export_graph_to_csv(world, viz_id);
    }

    if has_exp && has_live {
        let avail = ui.available_height();
        ui.allocate_ui(egui::vec2(ui.available_width(), avail * 0.5), |ui| {
            crate::ui::panels::experiments::render_experiments_plot(ui, world, viz_id);
        });
        ui.separator();
        render_line_plot(ui, world, viz_id);
    } else if has_exp {
        crate::ui::panels::experiments::render_experiments_plot(ui, world, viz_id);
    } else if has_live {
        render_line_plot(ui, world, viz_id);
    }
}

fn render_line_plot(ui: &mut egui::Ui, world: &mut World, viz_id: VizId) {
    let config = match world.resource::<VisualizationRegistry>().get(viz_id) {
        Some(c) => c.clone(),
        None => return,
    };
    let viz = LinePlot;
    let mut ctx = Panel2DCtx { ui, world };
    viz.render_panel_2d(&mut ctx, &config);
}

/// Gather the plot's bound signals, pop a native save-file picker,
/// and write a CSV with `time` + one column per signal.
fn export_graph_to_csv(world: &mut World, viz_id: VizId) {
    let (signals, labels) = {
        let Some(reg) = world.get_resource::<VisualizationRegistry>() else { return };
        let Some(cfg) = reg.get(viz_id) else { return };
        let sigs: Vec<_> = cfg.inputs.iter().map(|b| b.source.clone()).collect();
        let labels: Vec<String> = cfg
            .inputs
            .iter()
            .map(|b| b.label.clone().unwrap_or_else(|| b.source.path.clone()))
            .collect();
        (sigs, labels)
    };
    if signals.is_empty() {
        return;
    }

    let csv = {
        let Some(reg) = world.get_resource::<SignalRegistry>() else { return };
        export_signals_to_csv(reg, &signals, &labels)
    };

    let storage = lunco_storage::FileStorage::new();
    let hint = lunco_storage::SaveHint {
        suggested_name: Some(format!("modelica_plot_{}.csv", viz_id.0)),
        start_dir: None,
        filters: vec![lunco_storage::OpenFilter::new("CSV", &["csv"])],
    };
    let handle = match <lunco_storage::FileStorage as lunco_storage::Storage>::pick_save(
        &storage, &hint,
    ) {
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

    if let Err(e) = <lunco_storage::FileStorage as lunco_storage::Storage>::write(
        &storage,
        &handle,
        csv.as_bytes(),
    ) {
        if let Some(mut console) =
            world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
        {
            console.error(format!("CSV export: write failed: {e}"));
        }
    } else if let Some(mut console) =
        world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
    {
        let path = match &handle {
            lunco_storage::StorageHandle::File(p) => p.display().to_string(),
            _ => "(handle)".to_string(),
        };
        console.info(format!(
            "Exported {} bytes ({} signals) to {path}",
            csv.len(),
            signals.len()
        ));
    }
}
