//! Canvas scene rendering and event routing.

use bevy::prelude::*;
use bevy_egui::egui;
use crate::ui::panels::model_view::TabRenderContext;
use super::super::{CanvasDiagramState, CanvasSnapSettings, ops, overlays};
use super::super::theme::{CanvasThemeSnapshot, layer_theme_from, store_canvas_theme, store_modelica_icon_palette};
use super::util::{mark, log_frame_times};
use super::snapshots::stash_snapshots;
use super::interaction::{handle_context_menu, handle_drag_and_drop, handle_node_double_click};

pub(crate) fn render_diagram_canvas(
    _panel: &super::CanvasDiagramPanel,
    ui: &mut egui::Ui,
    world: &mut World,
) {
    let _frame_t0 = web_time::Instant::now();
    let render_tab_id = world.resource::<TabRenderContext>().tab_id;
    let trace_phases = std::env::var_os("RENDER_CANVAS_TRACE").is_some();
    let mut phase_t = web_time::Instant::now();
    let mut phase_log = Vec::new();

    let (doc_id, editing_class) = ops::resolve_doc_context(world);
    mark("resolve_doc_context", &mut phase_t, &mut phase_log);

    let active_doc = doc_id;
    let tab_read_only = active_doc.map(|d| crate::ui::state::read_only_for(world, d)).unwrap_or(false);

    let snap_settings = world.get_resource::<CanvasSnapSettings>().filter(|s| s.enabled).map(|s| lunco_canvas::SnapSettings { step: s.step });

    {
        let theme = world.get_resource::<lunco_theme::Theme>().cloned().unwrap_or_else(lunco_theme::Theme::dark);
        store_canvas_theme(ui.ctx(), CanvasThemeSnapshot::from_theme(&theme));
        store_modelica_icon_palette(ui.ctx(), theme.modelica_icons.clone());
        lunco_canvas::theme::store(ui.ctx(), layer_theme_from(&theme));
    }

    stash_snapshots(ui.ctx(), world, doc_id);
    mark("snapshots+sigreg", &mut phase_t, &mut phase_log);

    let (response, events) = {
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = match (render_tab_id, active_doc) { (Some(t), Some(d)) => state.get_mut_for_tab(t, d), _ => state.get_mut(active_doc) };
        docstate.canvas.read_only = tab_read_only;
        docstate.canvas.snap = snap_settings;
        docstate.canvas.ui(ui)
    };
    mark("canvas.ui (scene render)", &mut phase_t, &mut phase_log);

    if let Some(mut active) = world.get_resource_mut::<crate::ui::wasm_autosave::IsGestureActive>() {
        active.canvas = response.is_pointer_button_down_on();
    }

    // ─── Overlays ───
    {
        let theme = world.get_resource::<lunco_theme::Theme>().cloned().unwrap_or_else(lunco_theme::Theme::dark);
        let (load_error, show_indicator, show_empty) = {
            let tabs = world.resource::<crate::ui::panels::model_view::ModelTabs>();
            let err = render_tab_id.and_then(|tid| tabs.get(tid).and_then(|t| t.load_error.as_ref().map(|e| (t.drilled_class.clone().unwrap_or_default(), e.clone()))));
            
            let state = world.resource::<CanvasDiagramState>();
            let docstate = match render_tab_id { Some(t) => state.get_for_tab(t), None => state.get(active_doc) };
            let has_content = docstate.canvas.scene.node_count() > 0;
            let projecting = docstate.projection_task.is_some();
            let parse_pending = active_doc.and_then(|d| world.resource::<crate::ui::state::ModelicaDocumentRegistry>().host(d).map(|h| h.document().ast_is_stale())).unwrap_or(false);
            
            let openings = world.resource::<crate::ui::document_openings::DocumentOpenings>();
            let loading = active_doc.map(|d| openings.is_loading(d)).unwrap_or(false);

            (err, !has_content && (loading || parse_pending || projecting), !has_content && !loading && !parse_pending && !projecting)
        };

        if let Some((class, err)) = load_error {
            overlays::render_drill_in_error_overlay(ui, response.rect, &class, &err, &theme);
        } else if show_indicator {
            let bus = world.resource::<lunco_workbench::status_bus::StatusBus>();
            if let Some(doc_id) = active_doc {
                lunco_ui::busy::LoadingIndicator::for_scope(lunco_workbench::status_bus::BusyScope::Document(doc_id.0))
                    .overlay_on(ui, response.rect, bus, &theme);
            }
            ui.ctx().request_repaint();
        } else if show_empty {
            overlays::render_empty_diagram_overlay(ui, response.rect, world);
        }
    }

    handle_drag_and_drop(ui, world, &response, active_doc, render_tab_id, tab_read_only, editing_class.clone());
    let menu_ops = handle_context_menu(ui, world, &response, active_doc, render_tab_id, tab_read_only, editing_class.as_deref());
    handle_node_double_click(world, &events, active_doc);

    if let (Some(doc_id), Some(class)) = (doc_id, editing_class.as_ref()) {
        let mut all_ops = ops::build_ops_from_events(world, &events, class);
        all_ops.extend(menu_ops);
        if !all_ops.is_empty() {
            #[cfg(feature = "lunco-api")]
            crate::api::trigger_apply_ops(world, doc_id, all_ops);
            #[cfg(not(feature = "lunco-api"))]
            super::super::ops::apply_ops_public(world, doc_id, all_ops);
        }
    }

    // Apply in-canvas input-control widget writes. The control widget
    // (sliders rendered next to component icons) queues writes during
    // paint; we drain after the canvas finishes rendering so the
    // simulator's `model.inputs` map (and worker `set_input`) update
    // continuously while the user drags the slider.
    if let Some(doc_id) = active_doc {
        let writes = lunco_viz::kinds::canvas_plot_node::drain_input_writes(ui.ctx());
        for (name, value) in writes {
            if let Err(err) = crate::ui::commands::sim::apply_set_model_input(
                world, doc_id, &name, value,
            ) {
                bevy::log::warn!(
                    "[CanvasDiagram] in-canvas input write failed: name={name} value={value} err={err:?}"
                );
            }
        }
    }
    
    mark("tail (events/menu/fit)", &mut phase_t, &mut phase_log);
    log_frame_times(_frame_t0.elapsed().as_secs_f64() * 1000.0, 0.0);

    if trace_phases && !phase_log.is_empty() {
        let total: f64 = phase_log.iter().map(|(_, ms)| *ms).sum();
        if total > 30.0 {
            let breakdown = phase_log.iter().map(|(name, ms)| format!("{name}={ms:.1}ms")).collect::<Vec<_>>().join(" ");
            bevy::log::info!("[CanvasDiagram] render_canvas phases (sum={total:.1}ms): {breakdown}");
        }
    }
}
