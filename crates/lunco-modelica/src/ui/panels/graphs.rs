//! Modelica Graphs panel — workspace-layout entry point for the
//! singleton "Modelica" plot.
//!
//! All state and rendering live in `lunco-viz`; this panel is the
//! workbench-side wiring that:
//!
//! 1. Reserves the `modelica_graphs` slot in the bottom dock.
//! 2. Renders a small toolbar (Fit + count).
//! 3. Delegates the plot to [`LinePlot::render_panel_2d`] reading the
//!    [`DEFAULT_MODELICA_GRAPH`](crate::ui::viz::DEFAULT_MODELICA_GRAPH)
//!    config.
//!
//! No shadow state, no per-frame syncing — Telemetry writes directly
//! to the same config and the worker pushes samples into the same
//! `SignalRegistry`. Adding multiple plots is a future feature: open
//! a new `VizPanel` instance via `OpenTab { kind: VIZ_PANEL_KIND, .. }`.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_workbench::{Panel, PanelId, PanelSlot};
use lunco_viz::{
    export_signals_to_csv, kinds::line_plot::LinePlot, view::Panel2DCtx, viz::Visualization,
    SignalRegistry, VisualizationRegistry, VizFitRequests,
};

use crate::ui::viz::{ensure_default_modelica_graph, DEFAULT_MODELICA_GRAPH};

pub struct GraphsPanel;

impl Panel for GraphsPanel {
    fn id(&self) -> PanelId { PanelId("modelica_graphs") }
    fn title(&self) -> String { "📈 Graphs".into() }
    fn default_slot(&self) -> PanelSlot { PanelSlot::Bottom }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        let muted = world
            .get_resource::<lunco_theme::Theme>()
            .map(|t| t.tokens.text_subdued)
            .unwrap_or(egui::Color32::DARK_GRAY);
        // Take a cheap snapshot of everything the toolbar needs so we
        // can also render the plot in the same frame without
        // re-borrowing resources.
        let (bound_count, time_min, time_max, sample_total) = {
            let Some(mut registry) = world.get_resource_mut::<VisualizationRegistry>()
            else {
                ui.label("lunco-viz not installed.");
                return;
            };
            let cfg = ensure_default_modelica_graph(&mut registry);
            let count = cfg.inputs.len();
            let sources: Vec<_> = cfg.inputs.iter().map(|b| b.source.clone()).collect();
            drop(registry);

            // Time-range readout across all bound signals — the most
            // useful single number on a time-series plot. Falls back
            // to NaN when no data, handled by the label below.
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

        // Toolbar — data on the left, controls on the right. The
        // Fit button is a compact icon so the row is actually useful
        // for telemetry readouts, not empty space around one button.
        let mut fit_clicked = false;
        let mut export_csv_clicked = false;
        let exp_summary = crate::ui::panels::experiments::experiments_plot_summary(world);
        let has_live = bound_count > 0;
        // Render the experiments plot section as long as there's at
        // least one *run* (not just one drawn series). Without this
        // the inline variable picker disappears the moment the user
        // unticks the last variable, leaving them with no way to
        // re-enable plotting.
        let has_exp = exp_summary.total_runs > 0;

        // Single consolidated header line — live var count + exp run
        // status + time range + right-aligned controls. Replaces the
        // previous "0 var" + "Live cosim: …" + "Experiments · …" stack
        // of dead lines when nothing is selected.
        ui.horizontal(|ui| {
            // Live cosim summary
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
            }
            // Experiments summary (only when there are runs)
            if exp_summary.total_runs > 0 {
                if has_live {
                    ui.separator();
                }
                ui.label(
                    egui::RichText::new(format!(
                        "Experiments: {} run, {} visible, {} series",
                        exp_summary.total_runs,
                        exp_summary.visible_runs,
                        exp_summary.series_drawn
                    ))
                    .size(11.0)
                    .color(muted),
                );
            }
            // Empty-state hint on the same line
            if !has_live && exp_summary.total_runs == 0 {
                ui.label(
                    egui::RichText::new(
                        "No data yet — pick variables in Telemetry and run a model.",
                    )
                    .size(11.0)
                    .color(muted),
                );
            } else if !has_live && exp_summary.picked_vars == 0 {
                ui.label(
                    egui::RichText::new("Pick variables in Telemetry to plot.")
                        .size(11.0)
                        .color(muted),
                );
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let new_plot = ui
                    .small_button("➕")
                    .on_hover_text(
                        "New plot panel (➕) — opens a fresh tab so you can plot \
                         a different signal set side-by-side with this one",
                    );
                if new_plot.clicked() {
                    world
                        .commands()
                        .trigger(crate::ui::commands::NewPlotPanel::default());
                }
                let fit = ui
                    .small_button("📐")
                    .on_hover_text("Auto-fit (📐) — rescale axes to current data");
                if fit.clicked() {
                    fit_clicked = true;
                }
                let csv = ui
                    .small_button("💾 CSV")
                    .on_hover_text(
                        "Export CSV — save the live plot's signal histories to a \
                         CSV file (time column + one column per bound signal; \
                         forward-filled at union timestamps)",
                    );
                if csv.clicked() {
                    export_csv_clicked = true;
                }
            });
        });
        ui.separator();
        if fit_clicked {
            if let Some(mut requests) = world.get_resource_mut::<VizFitRequests>() {
                requests.request(DEFAULT_MODELICA_GRAPH);
            }
        }
        if export_csv_clicked {
            export_default_graph_to_csv(world);
        }

        // Plot area: when both live and experiments have content,
        // stack them; otherwise whichever has data fills the panel.
        if has_exp && has_live {
            // Split available space — experiments first (the user just
            // ran something) then live below.
            let avail = ui.available_height();
            ui.allocate_ui(egui::vec2(ui.available_width(), avail * 0.5), |ui| {
                crate::ui::panels::experiments::render_experiments_plot(ui, world);
            });
            ui.separator();
            let config = match world.resource::<VisualizationRegistry>().get(DEFAULT_MODELICA_GRAPH) {
                Some(c) => c.clone(),
                None => return,
            };
            let viz = LinePlot;
            let mut ctx = Panel2DCtx { ui, world };
            viz.render_panel_2d(&mut ctx, &config);
        } else if has_exp {
            crate::ui::panels::experiments::render_experiments_plot(ui, world);
        } else if has_live {
            let config = match world.resource::<VisualizationRegistry>().get(DEFAULT_MODELICA_GRAPH) {
                Some(c) => c.clone(),
                None => return,
            };
            let viz = LinePlot;
            let mut ctx = Panel2DCtx { ui, world };
            viz.render_panel_2d(&mut ctx, &config);
        }
        // else: header line shows the empty-state hint already.
    }
}

/// Gather the default plot's bound signals, pop a native save-file
/// picker, and write a CSV with `time` + one column per signal.
///
/// Goes through `lunco_storage::FileStorage` so the same call site
/// works when an OPFS / IndexedDB backend lands for wasm. Cancelling
/// the picker is a silent no-op; write errors go to the console.
fn export_default_graph_to_csv(world: &mut World) {
    let (signals, labels) = {
        let Some(reg) = world.get_resource::<VisualizationRegistry>() else { return };
        let Some(cfg) = reg.get(DEFAULT_MODELICA_GRAPH) else { return };
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
        suggested_name: Some("modelica_signals.csv".to_string()),
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
