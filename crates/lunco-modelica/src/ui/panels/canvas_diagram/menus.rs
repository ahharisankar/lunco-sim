//! Canvas diagram context menus + plot-node insertion.
//!
//! Right-click handlers that render the per-target context menu over
//! a node, edge, plot widget, or empty canvas. All menu actions
//! mutate panel state via the shared `CanvasDiagramState` resource
//! and emit `ModelicaOp` writes through `apply_ops_public` from the
//! parent module.

use bevy::prelude::*;
use bevy_egui::egui;

use crate::document::ModelicaOp;

use super::ops::{component_headers, op_remove_component, op_remove_edge};
use super::palette::{self, PaletteSettings};
use super::{CanvasDiagramState, active_doc_from_world};
use crate::ui::panels::model_view::TabRenderContext;

/// Read the active tab id from `TabRenderContext`. `None` when called
/// outside a panel render call (observers, off-render systems);
/// callers fall back to first-tab semantics in that case via
/// `CanvasDiagramState::get_for_render`.
fn render_tab_id(world: &World) -> Option<crate::ui::panels::model_view::TabId> {
    world
        .get_resource::<TabRenderContext>()
        .and_then(|c| c.tab_id)
}

pub(super) fn render_node_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    id: lunco_canvas::NodeId,
    editing_class: Option<&str>,
    out: &mut Vec<ModelicaOp>,
) {
    // Plot nodes are scene-only (no Modelica counterpart) — show a
    // signal-binding submenu and a Delete entry, skip the component-
    // specific actions (Open class, Parameters, Duplicate).
    let node_kind: Option<String> = {
        let active_doc = active_doc_from_world(world);
        let tab = render_tab_id(world);
        let state = world.resource::<CanvasDiagramState>();
        state
            .get_for_render(tab, active_doc)
            .canvas
            .scene
            .node(id)
            .map(|n| n.kind.to_string())
    };
    if node_kind.as_deref()
        == Some(lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND)
    {
        render_plot_node_menu(ui, world, id);
        return;
    }
    let (instance, type_name) = component_headers(world, id);
    ui.label(egui::RichText::new(&instance).strong());
    if !type_name.is_empty() {
        ui.label(egui::RichText::new(&type_name).weak().small());
    }
    ui.separator();
    if ui.button("✂ Delete").clicked() {
        if let Some(class) = editing_class {
            if let Some(op) = op_remove_component(world, id, class) {
                out.push(op);
                // Optimistic scene mutation — `apply_ops` will then
                // bump `canvas_acked_gen` and the project gate skips
                // the redundant reproject.
                let active_doc = active_doc_from_world(world);
                let tab = render_tab_id(world);
                let mut state = world.resource_mut::<CanvasDiagramState>();
                let docstate = state.get_mut_for_render(tab, active_doc);
                docstate.canvas.scene.remove_node(id);
            }
        }
        ui.close();
    }
    if ui.button("📋 Duplicate").clicked() {
        ui.close();
    }
    ui.separator();
    if ui.button("↧ Open class").clicked() {
        ui.close();
    }
    if ui.button("🔧 Parameters…").clicked() {
        ui.close();
    }
}

pub(super) fn render_plot_node_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    id: lunco_canvas::NodeId,
) {
    use lunco_viz::kinds::canvas_plot_node::PlotNodeData;

    let current: PlotNodeData = {
        let active_doc = active_doc_from_world(world);
        let tab = render_tab_id(world);
        let state = world.resource::<CanvasDiagramState>();
        state
            .get_for_render(tab, active_doc)
            .canvas
            .scene
            .node(id)
            .and_then(|n| n.data.downcast_ref::<PlotNodeData>().cloned())
            .unwrap_or_default()
    };
    ui.label(egui::RichText::new("Plot").strong());
    if !current.signal_path.is_empty() {
        ui.label(
            egui::RichText::new(&current.signal_path)
                .weak()
                .small(),
        );
    } else {
        ui.label(
            egui::RichText::new("(unbound)")
                .weak()
                .small()
                .italics(),
        );
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

    ui.menu_button("🔗 Bind signal", |ui| {
        if sigs.is_empty() {
            ui.label(
                egui::RichText::new("(no signals yet — run a simulation)")
                    .weak()
                    .small(),
            );
            return;
        }
        let max_h = ui.ctx().screen_rect().height() * 0.7;
        egui::ScrollArea::vertical()
            .max_height(max_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (entity, path) in &sigs {
                    let is_current = entity.to_bits() == current.entity
                        && path == &current.signal_path;
                    if ui.selectable_label(is_current, path).clicked() {
                        rebind_plot_node(world, id, entity.to_bits(), path);
                        ui.close();
                    }
                }
            });
    });

    if !current.signal_path.is_empty() && ui.button("Unbind").clicked() {
        rebind_plot_node(world, id, 0, "");
        ui.close();
    }
    ui.separator();
    if ui.button("✂ Delete").clicked() {
        let active_doc = active_doc_from_world(world);
        let tab = render_tab_id(world);
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = state.get_mut_for_render(tab, active_doc);
        docstate.canvas.scene.remove_node(id);
        ui.close();
    }
}

pub(super) fn rebind_plot_node(
    world: &mut World,
    id: lunco_canvas::NodeId,
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
    let active_doc = active_doc_from_world(world);
    let tab = render_tab_id(world);
    let mut state = world.resource_mut::<CanvasDiagramState>();
    let docstate = state.get_mut_for_render(tab, active_doc);
    if let Some(node) = docstate.canvas.scene.node_mut(id) {
        node.data = data;
    }
}

pub(super) fn render_edge_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    id: lunco_canvas::EdgeId,
    editing_class: Option<&str>,
    out: &mut Vec<ModelicaOp>,
) {
    ui.label(egui::RichText::new("Connection").strong());
    ui.separator();
    if ui.button("✂ Delete").clicked() {
        if let Some(class) = editing_class {
            if let Some(op) = op_remove_edge(world, id, class) {
                out.push(op);
                let active_doc = active_doc_from_world(world);
                let tab = render_tab_id(world);
                let mut state = world.resource_mut::<CanvasDiagramState>();
                let docstate = state.get_mut_for_render(tab, active_doc);
                docstate.canvas.scene.remove_edge(id);
            }
        }
        ui.close();
    }
    if ui.button("↺ Reverse direction").clicked() {
        ui.close();
    }
}

pub(super) fn render_empty_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    click_world: lunco_canvas::Pos,
    editing_class: Option<&str>,
    out: &mut Vec<ModelicaOp>,
) {
    ui.label(egui::RichText::new("Add component").strong());
    ui.separator();

    // Hierarchical package navigation — each submenu level mirrors
    // Modelica's package tree (Modelica → Electrical → Analog →
    // Basic → Resistor). Matches how OMEdit and Dymola present
    // the library: user drills down by package instead of
    // scanning a flat list. Tree is built once, cached.
    let show_icons = world
        .get_resource::<PaletteSettings>()
        .map(|s| s.show_icon_only_classes)
        .unwrap_or(false);
    let active_doc = active_doc_from_world(world);
    palette::render_msl_package_menu(
        ui,
        world,
        active_doc,
        palette::msl_package_tree(),
        click_world,
        editing_class,
        show_icons,
        out,
    );
    ui.separator();
    // ── Add Plot ──────────────────────────────────────────────────
    // In-canvas scope: drop a `lunco.viz.plot` Scene node at the click
    // position. The "Empty plot" entry is always available so users
    // can place a chart while authoring, before any simulation has
    // run; signal entries appear once the active sim has populated
    // `SignalRegistry`. An empty plot can be bound later via the
    // inspector.
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
    ui.menu_button("📊 Add Plot here", |ui| {
        // TODO(menu-height): the height is "so-so" — sometimes
        // collapses to 3 rows. Match how the Modelica
        // "Add component" cascade works (see
        // `render_msl_package_menu` ~3065): plain
        // `ui.menu_button(..., |ui| ...)` recursively, no explicit
        // `set_min_*`/`set_max_*`. Egui auto-sizes from content
        // there and it Just Works. The current adaptive
        // computation below is a workaround — the real fix is to
        // mirror that simpler structure (probably means dropping
        // the ScrollArea wrapper too).
        const ROW_PX: f32 = 18.0;
        let max_h = (ui.ctx().screen_rect().height() * 0.7).max(180.0);
        let wanted = ((sigs.len() + 3) as f32 * ROW_PX).min(max_h);
        ui.set_min_height(wanted);
        if ui.button("Empty plot (bind later)").clicked() {
            insert_plot_node(world, click_world, 0, "");
            ui.close();
        }
        ui.separator();
        if sigs.is_empty() {
            ui.label(
                egui::RichText::new("(no signals yet — run a simulation to bind)")
                    .weak()
                    .small(),
            );
            return;
        }
        // ScrollArea caps the height at 80 % of the screen so the
        // popup never spills past the window. `auto_shrink: true`
        // for height — the popup itself only grows as tall as it
        // needs. `false` for width so long names don't trigger a
        // horizontal scrollbar.
        let max_h = ui.ctx().screen_rect().height() * 0.8;
        egui::ScrollArea::vertical()
            .max_height(max_h)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (entity, path) in &sigs {
                    if ui.button(path).clicked() {
                        insert_plot_node(world, click_world, entity.to_bits(), path);
                        ui.close();
                    }
                }
            });
    });
    ui.separator();
    if ui.button("⎚ Fit all (F)").clicked() {
        let active_doc = active_doc_from_world(world);
        let tab = render_tab_id(world);
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = state.get_mut_for_render(tab, active_doc);
        if let Some(bounds) = docstate.canvas.scene.bounds() {
            let sr = lunco_canvas::Rect::from_min_max(
                lunco_canvas::Pos::new(0.0, 0.0),
                lunco_canvas::Pos::new(800.0, 600.0),
            );
            let (c, z) = docstate.canvas.viewport.fit_values(bounds, sr, 40.0);
            docstate.canvas.viewport.set_target(c, z);
        }
        ui.close();
    }
    if ui.button("⟲ Reset zoom").clicked() {
        let active_doc = active_doc_from_world(world);
        let tab = render_tab_id(world);
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = state.get_mut_for_render(tab, active_doc);
        let c = docstate.canvas.viewport.center;
        docstate.canvas.viewport.set_target(c, 1.0);
        ui.close();
    }
}

pub(super) fn insert_plot_node(
    world: &mut World,
    click_world: lunco_canvas::Pos,
    entity_bits: u64,
    signal_path: &str,
) {
    let payload = lunco_viz::kinds::canvas_plot_node::PlotNodeData {
        entity: entity_bits,
        signal_path: signal_path.to_string(),
        title: String::new(),
    };
    let data: lunco_canvas::NodeData = std::sync::Arc::new(payload);
    let active_doc = active_doc_from_world(world);
    let tab = render_tab_id(world);
    let mut state = world.resource_mut::<CanvasDiagramState>();
    let docstate = state.get_mut_for_render(tab, active_doc);
    let scene = &mut docstate.canvas.scene;
    let id = scene.alloc_node_id();
    scene.insert_node(lunco_canvas::scene::Node {
        id,
        rect: lunco_canvas::Rect::from_min_max(
            click_world,
            lunco_canvas::Pos::new(click_world.x + 60.0, click_world.y + 40.0),
        ),
        kind: lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND.into(),
        data,
        ports: Vec::new(),
        label: String::new(),
        origin: None,
        resizable: true,
        visual_rect: None,
    });
}