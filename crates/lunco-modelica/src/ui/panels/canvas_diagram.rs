//! Modelica diagram, rendered via `lunco-canvas`.
//!
//! Sole diagram path. The previous egui-snarl-backed `diagram.rs`
//! has been removed — `lunco-canvas` covers every feature we use.
//!
//! # Pipeline
//!
//! ```text
//!   ModelicaDocument (AST)                        (lunco-doc)
//!           │
//!           ▼
//!   VisualDiagram  (existing intermediate)        (lunco-modelica)
//!           │  project_scene()
//!           ▼
//!   lunco_canvas::Scene   →  Canvas   →  egui
//!           ▲                  │
//!           └──── SceneEvent ──┘      → (future) DocumentOp back to ModelicaDocument
//! ```
//!
//! # What's in B2
//!
//! - Read-side projector: `VisualDiagram → Scene` (one-shot on bind,
//!   rebuilt on doc generation change).
//! - Rectangle + label visuals; straight-line edges.
//! - Drag-to-move nodes → mutates the local scene (feedback only —
//!   doc ops from drag land in B3).
//! - Pan / zoom / select / rubber-band / Delete / F-to-fit — all via
//!   the default `Canvas` wiring, nothing to wire here.
//!
//! Icon rendering (SVG via `usvg`), animated wires, widget-in-node
//! plots, and doc-op emission all land later as new visual impls /
//! in the projector's write-back path — no canvas changes required.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_canvas::{
    Canvas, DrawCtx, Edge as CanvasEdge, EdgeVisual, NavBarOverlay, Node as CanvasNode,
    NodeId as CanvasNodeId, NodeVisual, Pos as CanvasPos, Port as CanvasPort,
    PortId as CanvasPortId, PortRef, Rect as CanvasRect, Scene, VisualRegistry,
};
use lunco_workbench::{Panel, PanelId, PanelSlot};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

use crate::ui::state::{ModelicaDocumentRegistry, WorkbenchState};
use crate::ui::theme::ModelicaThemeExt;
use crate::visual_diagram::{DiagramNodeId, MSLComponentDef, VisualDiagram};
// `Document` is the trait that exposes `.generation()` on
// `ModelicaDocument`; `DocumentHost::document()` returns a bare `&D`
// so we need the trait in scope to call generation on it.
use lunco_doc::Document;
// Modelica op set + pretty-printer types for constructing payloads.
use crate::document::ModelicaOp;
use crate::pretty::{self, Placement};

pub const CANVAS_DIAGRAM_PANEL_ID: PanelId = PanelId("modelica_canvas_diagram");

// ─── Visuals ────────────────────────────────────────────────────────

/// Theme-derived colour snapshot consumed by every layer inside the
/// canvas this frame. Stashed in the egui context's data cache (by
/// type) at the entry of [`CanvasDiagramPanel::render`] so the
/// [`NodeVisual`] / [`EdgeVisual`] trait objects — which have no
/// `World` access — can still pick theme-aware colours on draw.
///
/// Recomputed each frame; cloning is a handful of `Color32` copies.
#[derive(Clone, Debug)]
pub struct CanvasThemeSnapshot {
    pub card_fill: egui::Color32,
    pub node_label: egui::Color32,
    pub type_label: egui::Color32,
    pub port_fill: egui::Color32,
    pub port_stroke: egui::Color32,
    pub select_stroke: egui::Color32,
    pub inactive_stroke: egui::Color32,
    pub icon_only_stroke: egui::Color32,
    /// When false (default), authored MSL icons render without a
    /// workbench-drawn hairline frame around them. The icon's own
    /// primitives are the bounds. Selection / icon-only / expandable
    /// rings still draw — they carry semantic info, not just bounds.
    pub show_authored_icon_border: bool,
}

impl CanvasThemeSnapshot {
    pub fn from_theme(theme: &lunco_theme::Theme) -> Self {
        let c = &theme.colors;
        let t = &theme.tokens;
        let s = &theme.schematic;
        Self {
            // Card background tuned to contrast cleanly with the
            // blue-heavy MSL icon palette (Modelica Blocks / many
            // Electrical components use strong blues). Delegates to
            // the theme's dedicated `canvas_card` schematic token —
            // see `lunco_theme::SchematicTokens::canvas_card`.
            card_fill: s.canvas_card,
            node_label: t.text,
            type_label: t.text_subdued,
            port_fill: c.overlay1,
            port_stroke: c.surface2,
            // (c still referenced below for ports/selection; keep)
            // Selection follows `tokens.accent` so the active-icon
            // ring matches the rest of the app's accent chrome.
            select_stroke: t.accent,
            // Idle border: muted edge, same intent as the faint
            // outline around any inactive widget.
            inactive_stroke: c.overlay0,
            // Icon-only ring uses `warning` — signals "this is
            // decorative, doesn't carry connectors" via the same
            // colour the app uses for other cautionary chrome.
            icon_only_stroke: t.warning,
            show_authored_icon_border: false,
        }
    }
}

/// Fetch the theme snapshot stored for this frame by the canvas
/// render entry. `None` when the canvas is rendered outside our
/// panel (tests / demos); caller falls back to a default snapshot
/// derived from `Theme::dark()`.
fn canvas_theme_from_ctx(ctx: &egui::Context) -> CanvasThemeSnapshot {
    let id = egui::Id::new("lunco.modelica.canvas_theme_snapshot");
    ctx.data(|d| d.get_temp::<CanvasThemeSnapshot>(id))
        .unwrap_or_else(|| {
            CanvasThemeSnapshot::from_theme(&lunco_theme::Theme::dark())
        })
}

/// Build the generic `lunco_canvas` layer theme (grid, selection halo,
/// tool preview, zoom-bar overlay) from the active LunCoSim theme.
/// Pushed to the canvas each frame so its built-in layers render in
/// palette-matched colours instead of their hardcoded dark defaults.
fn layer_theme_from(theme: &lunco_theme::Theme) -> lunco_canvas::CanvasLayerTheme {
    let c = &theme.colors;
    let t = &theme.tokens;
    // Grid: dim overlay dot. Using overlay0 at low alpha reads on
    // both Mocha (dark) and Latte (light) without competing with
    // diagram content.
    let grid = {
        let g = c.overlay0;
        egui::Color32::from_rgba_unmultiplied(g.r(), g.g(), g.b(), 60)
    };
    let rubber_fill = {
        let a = t.accent;
        egui::Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 40)
    };
    let shadow = {
        let b = c.base;
        egui::Color32::from_rgba_unmultiplied(b.r(), b.g(), b.b(), 110)
    };
    lunco_canvas::CanvasLayerTheme {
        grid,
        selection_outline: t.accent,
        ghost_edge: t.accent,
        snap_target: t.success,
        rubber_band_fill: rubber_fill,
        rubber_band_stroke: t.accent,
        overlay_fill: c.surface0,
        overlay_stroke: c.surface2,
        overlay_shadow: shadow,
        overlay_text: t.text,
    }
}

/// Store a theme snapshot in the egui data cache under a well-known
/// id. Counterpart to [`canvas_theme_from_ctx`].
fn store_canvas_theme(ctx: &egui::Context, snap: CanvasThemeSnapshot) {
    let id = egui::Id::new("lunco.modelica.canvas_theme_snapshot");
    ctx.data_mut(|d| d.insert_temp(id, snap));
}

/// Stash the active theme's Modelica icon palette in the egui data
/// cache so leaf paint helpers (running outside the Bevy world) can
/// remap authored MSL colors to fit the active theme. Read-side:
/// [`modelica_icon_palette_from_ctx`].
fn store_modelica_icon_palette(
    ctx: &egui::Context,
    palette: lunco_theme::ModelicaIconPalette,
) {
    let id = egui::Id::new("lunco.modelica.icon_palette");
    ctx.data_mut(|d| d.insert_temp(id, palette));
}

fn modelica_icon_palette_from_ctx(
    ctx: &egui::Context,
) -> Option<lunco_theme::ModelicaIconPalette> {
    let id = egui::Id::new("lunco.modelica.icon_palette");
    ctx.data(|d| d.get_temp::<lunco_theme::ModelicaIconPalette>(id))
}

/// Typed payload carried in `lunco_canvas::Node.data` for every
/// `"modelica.icon"` node. Replaces the prior `serde_json::Value`
/// round-trip — projector boxes one of these, the visual factory
/// downcasts at construction. The Modelica primitive types it
/// carries (`Icon`, parameters) all derive `Serialize`/`Deserialize`,
/// so a future Scene snapshot story can serialize this struct
/// directly via a per-domain registry.
#[derive(Clone, Debug, Default)]
pub struct IconNodeData {
    /// Fully-qualified type name (e.g. `Modelica.Electrical.Analog.Basic.Resistor`).
    pub qualified_type: String,
    /// `Icons.*` package class — rendered with a dashed border so
    /// users see at a glance the component is decorative.
    pub icon_only: bool,
    /// `expandable connector` (MLS §9.1.3) — accent dashed border.
    pub expandable_connector: bool,
    /// Decoded `Icon(graphics={...})` annotation merged across the
    /// `extends` chain. `None` only when the class has literally no
    /// Icon in inheritance — then the visual falls back to a label box.
    pub icon_graphics: Option<crate::annotations::Icon>,
    /// Decoded `Diagram(graphics={...})` annotation, populated only
    /// for connector classes that author one. When set the renderer
    /// uses this instead of `icon_graphics` — MSL signal connectors
    /// (RealInput, RealOutput, …) put the `%name` text label and
    /// the larger filled triangle in their Diagram annotation, while
    /// keeping a stripped-down Icon for use as a port marker.
    pub diagram_graphics: Option<crate::annotations::Diagram>,
    /// Per-instance rotation (degrees CCW, Modelica convention).
    pub rotation_deg: f32,
    /// Mirror flags applied before rotation (MLS Annex D order).
    pub mirror_x: bool,
    pub mirror_y: bool,
    /// Instance name — drives `%name` text substitution.
    pub instance_name: String,
    /// Pre-formatted `(param_name, value)` for `%paramName` text
    /// substitution. Class defaults today; instance modifications
    /// follow.
    pub parameters: Vec<(String, String)>,
    /// Per-port connector-icon descriptors: `(port_name,
    /// connector_class_qualified_path, size_x, size_y, rotation_deg)`.
    /// The painter renders each connector class's authored `Icon` at
    /// the port location, sized + rotated per the port's authored
    /// `Placement(transformation(extent=..., rotation=...))`. Empty
    /// path falls back to the generic per-shape marker.
    pub port_connector_paths: Vec<(String, String, f32, f32, f32)>,
    /// Conditional component (`Component X if <cond>`). Renderer
    /// halves opacity so users can see it's design-time visible but
    /// runtime-conditional — matches OMEdit/Dymola convention.
    pub is_conditional: bool,
}

/// Typed payload for `"modelica.connection"` edges. Same purpose as
/// [`IconNodeData`].
#[derive(Clone, Debug, Default)]
pub struct ConnectionEdgeData {
    pub connector_type: String,
    pub from_dir: PortDir,
    pub to_dir: PortDir,
    pub waypoints_world: Vec<lunco_canvas::Pos>,
    pub icon_color: Option<egui::Color32>,
    pub source_path: String,
    pub target_path: String,
    pub kind: crate::visual_diagram::PortKind,
    pub flow_vars: Vec<crate::visual_diagram::FlowVarMeta>,
}

/// Per-component icon visual. Renders, in priority order:
///
/// 1. The class's decoded `Icon(graphics={...})` annotation merged
///    across the `extends` chain — the only icon source. Painted via
///    [`crate::icon_paint::paint_graphics`] with lyon-tessellated
///    fills (EvenOdd, matching OMEdit/Dymola).
/// 2. A stylised rounded-rectangle fallback with the type label, used
///    only when the class has no `Icon` annotation anywhere in its
///    inheritance chain.
///
/// Ports render as filled dots on the icon boundary in all cases.
#[derive(Default)]
struct IconNodeVisual {
    /// Type name ("Resistor", "Capacitor"…) shown under the instance
    /// label when the class has no Icon at all.
    type_label: String,
    /// Pure-icon class (zero connectors, `.Icons.*` subpackage).
    /// Rendered with a dashed border so users can tell at a glance
    /// the component is decorative. Set by the projector via the
    /// node's `data.icon_only` flag.
    icon_only: bool,
    /// `expandable connector` class (MLS §9.1.3). Rendered with a
    /// dashed border in an accent colour so users can distinguish
    /// them from regular connectors — expandable connectors collect
    /// variables across connections dynamically and have different
    /// semantics.
    expandable_connector: bool,
    /// Decoded graphics from the class's `Icon` annotation. When
    /// present, takes precedence over the SVG icon path so user
    /// classes show their authored graphics instead of falling back
    /// to a generic placeholder.
    icon_graphics: Option<crate::annotations::Icon>,
    /// `Diagram(...)` graphics for connector instances rendered at
    /// top-level on a parent diagram. Preferred over `icon_graphics`
    /// when present.
    diagram_graphics: Option<crate::annotations::Diagram>,
    /// Conditional component flag — render dimmed.
    is_conditional: bool,
    /// Pre-formatted `(parameter_name, value)` pairs for `%paramName`
    /// text substitution. Carries class defaults from
    /// `MSLComponentDef.parameters` (instance-modification overlay
    /// is a follow-up — most icons display defaults anyway when no
    /// instance modifications are set).
    parameters: Vec<(String, String)>,
    /// Per-instance rotation (degrees CCW, Modelica frame) applied to
    /// the icon body itself — rotates both the SVG raster and the
    /// `paint_graphics` primitives uniformly. Without this, mirror /
    /// rotated MSL placements showed correct port positions but a
    /// wrong-looking body.
    rotation_deg: f32,
    /// Mirror flags applied to the icon body, before rotation
    /// (MLS Annex D).
    mirror_x: bool,
    mirror_y: bool,
    /// Instance name this component is drawn for — "R1", "C1", …
    /// Drives the `%name` substitution in authored `Text` primitives
    /// (Modelica's convention for showing the instance label on the
    /// icon body). Empty when the projector didn't provide one.
    instance_name: String,
    /// Class name (leaf — e.g. "Resistor"). Drives `%class`
    /// substitution in authored `Text` primitives.
    class_name: String,
    /// `(port_name, connector_class_qualified_path, size_x, size_y,
    /// rotation_deg)` from the projected scene.
    port_connector_paths: Vec<(String, String, f32, f32, f32)>,
    /// Parent component's fully-qualified type — used as the scope
    /// root when the indexer wrote a short connector path like
    /// `"RealInput"` and we need to resolve it via package walk.
    parent_qualified_type: String,
}

impl NodeVisual for IconNodeVisual {
    fn draw(&self, ctx: &mut DrawCtx, node: &CanvasNode, selected: bool) {
        let r = ctx
            .viewport
            .world_rect_to_screen(node.rect, ctx.screen_rect);
        let rect = egui::Rect::from_min_max(
            egui::pos2(r.min.x, r.min.y),
            egui::pos2(r.max.x, r.max.y),
        );
        let painter = ctx.ui.painter();
        let theme_snap = canvas_theme_from_ctx(ctx.ui.ctx());
        // Conditional components (`Component X if cond`) — render at
        // reduced opacity so every primitive (icon shapes, text,
        // port markers) inherits the dimming. Matches OMEdit/Dymola
        // convention for "design-time visible, runtime-conditional"
        // components.
        let _dimmed_painter;
        let painter: &egui::Painter = if self.is_conditional {
            let mut p = painter.clone();
            p.set_opacity(0.4);
            _dimmed_painter = p;
            &_dimmed_painter
        } else {
            painter
        };

        // No always-on card fill. Icons that need a body (Resistor's
        // white rectangle, Inertia's gray cylinder, …) author it
        // themselves; classes without an Icon at all get the
        // placeholder card from the `!drew_icon` branch below.
        // Matches Dymola/OMEdit — they never paint a "competing"
        // card behind authored icons.

        // Authored graphics from the class's `Icon` annotation,
        // merged across the `extends` chain at index time.
        // Per-instance orientation rotates+mirrors every primitive
        // at the rect level so placement-rotation shows visually,
        // not just on the port positions.
        let orientation = crate::icon_paint::IconOrientation {
            rotation_deg: self.rotation_deg,
            mirror_x: self.mirror_x,
            mirror_y: self.mirror_y,
        };
        let mut drew_icon = false;
        if let Some(icon) = &self.icon_graphics {
            let sub = crate::icon_paint::TextSubstitution {
                name: (!self.instance_name.is_empty()).then_some(self.instance_name.as_str()),
                class_name: (!self.class_name.is_empty()).then_some(self.class_name.as_str()),
                parameters: (!self.parameters.is_empty()).then_some(self.parameters.as_slice()),
            };
            // Build a per-instance value resolver for MLS §18
            // `DynamicSelect` text expressions. The icon expression
            // is written in the component's local scope (`m`,
            // `port.m_flow`); the live snapshot is keyed by full
            // instance path (`tank.m`, `tank.port.m_flow`). We
            // prefix with `instance_name.` and look it up — that
            // covers both top-level state vars and dotted refs into
            // sub-components / ports. Falls back to the bare name
            // for cases like global `time`.
            let node_state =
                lunco_viz::kinds::canvas_plot_node::fetch_node_state(ctx.ui.ctx());
            let instance = self.instance_name.clone();
            let resolver = move |name: &str| -> Option<f64> {
                if !instance.is_empty() {
                    let qualified = format!("{instance}.{name}");
                    if let Some(&v) = node_state.values.get(&qualified) {
                        return Some(v);
                    }
                }
                node_state.values.get(name).copied()
            };
            let resolver_ref: &dyn Fn(&str) -> Option<f64> = &resolver;
            let palette = modelica_icon_palette_from_ctx(ctx.ui.ctx());
            // Source coord system: prefer the icon's *graphics* bbox
            // (visible body, excluding labels) so the icon body fills
            // the placement instead of leaving 30–50 % empty padding
            // around it. MSL convention is to author at -100..100, but
            // many components actually draw at -50..50 / -60..60 etc.,
            // which makes them look small inside the standard
            // placement. Excluding text from the bbox is intentional:
            // the body should fill the rect; labels drift slightly
            // outside but get clipped by the canvas widget. Falls
            // back to the declared coord system when there are no
            // graphics.
            let coord_system_for_paint = icon
                .graphics_bbox()
                .map(|e| crate::annotations::CoordinateSystem { extent: e })
                .unwrap_or(icon.coordinate_system);
            crate::icon_paint::paint_graphics_themed(
                painter,
                rect,
                coord_system_for_paint,
                orientation,
                Some(&sub),
                Some(resolver_ref),
                palette.as_ref(),
                &icon.graphics,
            );
            drew_icon = true;
            // Overlay Diagram-annotation graphics — for MSL signal
            // connectors this carries the `%name` Text label and a
            // smaller decorative inner polygon. Painted on top of the
            // Icon so the name appears next to the connector triangle
            // without replacing the full-size Icon polygon.
            if let Some(diag) = &self.diagram_graphics {
                crate::icon_paint::paint_graphics_themed(
                    painter,
                    rect,
                    diag.coordinate_system,
                    orientation,
                    Some(&sub),
                    Some(resolver_ref),
                    palette.as_ref(),
                    &diag.graphics,
                );
            }
        }

        if !drew_icon {
            // Placeholder for classes with literally no `Icon` in
            // their extends chain — same shape as OMEdit's "no icon
            // authored yet" stand-in: rounded card + class name
            // centred. Once the user (or the indexer) authors an
            // Icon annotation, the live path above takes over and
            // we never run this fallback again.
            painter.rect_filled(rect, 6.0, theme_snap.card_fill);
            if !self.type_label.is_empty() && rect.height() > 30.0 {
                painter.text(
                    egui::pos2(rect.center().x, rect.center().y),
                    egui::Align2::CENTER_CENTER,
                    &self.type_label,
                    egui::FontId::proportional(10.0),
                    theme_snap.type_label,
                );
            }
        }

        // Border policy:
        //   - Selection ring: always drawn (functional feedback).
        //   - Icon-only / expandable connector accents: always drawn
        //     (carry semantic info — "decorative" / "expandable").
        //   - Placeholder card outline: always drawn (the card has
        //     no other body and would melt into the canvas otherwise).
        //   - Authored-icon hairline: opt-in via the theme snapshot's
        //     `show_authored_icon_border` flag, off by default. The
        //     icon's own primitives carry its bounds; the workbench
        //     hairline competed with them and was reported as visual
        //     noise. Power users can flip the flag in Settings later.
        let stroke = if selected {
            Some(egui::Stroke::new(2.0, theme_snap.select_stroke))
        } else if self.icon_only {
            Some(egui::Stroke::new(1.0, theme_snap.icon_only_stroke))
        } else if self.expandable_connector {
            Some(egui::Stroke::new(1.5, theme_snap.select_stroke))
        } else if !drew_icon {
            Some(egui::Stroke::new(1.0, theme_snap.inactive_stroke))
        } else if theme_snap.show_authored_icon_border {
            let c = theme_snap.inactive_stroke;
            let dim = egui::Color32::from_rgba_unmultiplied(
                c.r(),
                c.g(),
                c.b(),
                (c.a() / 3).max(40),
            );
            Some(egui::Stroke::new(0.75, dim))
        } else {
            None
        };
        if let Some(stroke) = stroke {
            let wants_dashed = (self.icon_only || self.expandable_connector) && !selected;
            if wants_dashed {
                paint_dashed_rect(painter, rect, 6.0, stroke);
            } else {
                painter.rect_stroke(rect, 6.0, stroke, egui::StrokeKind::Outside);
            }
        }

        // Instance name: deliberately NOT drawn here. Modelica icons
        // author their own `Text(textString="%name", extent={...})`
        // primitive — we substitute via `TextSubstitution` and the
        // icon decides where the name belongs. Drawing a workbench-
        // owned label here too produced the duplicate-name visual
        // noise users hit on the PID example. OMEdit / Dymola don't
        // draw an external label either.

        // Ports — shape per connector causality (OMEdit / Dymola
        // convention):
        //   • input  → filled square   (RealInput, BooleanInput, …)
        //   • output → filled triangle pointing outward
        //   • acausal physical → filled circle (Pin, Flange, HeatPort, …)
        // Direction is derived from where the port sits on the icon
        // boundary, classified the same way edges classify port_dir.
        for port in &node.ports {
            let world = CanvasPos::new(
                node.rect.min.x + port.local_offset.x,
                node.rect.min.y + port.local_offset.y,
            );
            let p = ctx.viewport.world_to_screen(world, ctx.screen_rect);
            // Pixel-snap so the marker centre aligns with the
            // wire endpoint (which is also snapped — see
            // `EdgesLayer::draw`). Without this, the wire end
            // and the port circle drift apart by up to 1 px on
            // some zoom levels.
            let center = egui::pos2(p.x.round(), p.y.round());

            let cx = node.rect.min.x + node.rect.width() * 0.5;
            let cy = node.rect.min.y + node.rect.height() * 0.5;
            let dir = port_edge_dir(world.x - cx, world.y - cy);

            // Try the OMEdit-parity path first: render the connector
            // class's authored `Icon` at the port location. Falls
            // through to the generic per-shape marker if the class
            // can't be resolved (rare — typically only when the MSL
            // pre-warm hasn't reached that connector yet) or the
            // class has no `Icon` annotation in its inheritance chain.
            let port_info = self
                .port_connector_paths
                .iter()
                .find(|(name, _, _, _, _)| name == port.id.as_str());
            let connector_path: &str = port_info
                .map(|(_, p, _, _, _)| p.as_str())
                .unwrap_or("");
            let (port_size_x_icon, port_size_y_icon, port_rotation_deg) = port_info
                .map(|(_, _, sx, sy, rot)| (*sx, *sy, *rot))
                .unwrap_or((20.0, 20.0, 0.0));
            let mut painted_authored = false;
            // The indexer ideally writes a fully-qualified path, but
            // older indexes wrote the type as-declared (`"RealInput"`)
            // — fall back to a scope-chain walk rooted at the parent
            // class so cached indexes still resolve. First hit wins.
            let parent_qualified = self.parent_qualified_type.as_str();
            let candidates: Vec<String> = if connector_path.contains('.') {
                vec![connector_path.to_string()]
            } else if !connector_path.is_empty() {
                let mut out = Vec::new();
                let mut scope = parent_qualified.to_string();
                while scope.contains('.') {
                    let pkg = scope.rsplitn(2, '.').nth(1).unwrap_or("").to_string();
                    if !pkg.is_empty() {
                        out.push(format!("{pkg}.Interfaces.{connector_path}"));
                        out.push(format!("{pkg}.{connector_path}"));
                    }
                    scope = pkg;
                }
                out.push(connector_path.to_string());
                out
            } else {
                Vec::new()
            };
            let resolved = candidates
                .into_iter()
                .find_map(|c| {
                    crate::class_cache::peek_or_load_msl_class(&c).map(|class| (c, class))
                });
            if let Some((resolved_path, class)) = resolved {
                use std::sync::Arc;
                let mut resolver = |lookup: &str| -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
                    crate::class_cache::peek_or_load_msl_class(lookup)
                };
                let mut visited = std::collections::HashSet::new();
                if let Some(icon) = crate::annotations::extract_icon_inherited(
                    &resolved_path,
                    class.as_ref(),
                    &mut resolver,
                    &mut visited,
                ) {
                        // Render the connector's icon at the port
                        // location, sized to the port's authored
                        // `Placement(extent=...)` in the parent's icon
                        // coords. MSL convention: parent icon coord
                        // system spans 200 units (-100..100) and the
                        // parent is placed at `node.rect` in world
                        // coords. So 1 icon-unit = node_world / 200.
                        // Connector placement (e.g. Flange_a's
                        // 20×20 box) maps to 20/200 * node_world =
                        // 10% of the parent's world width — the small
                        // dot OMEdit shows.
                        let parent_w = node.rect.width().max(1.0);
                        let parent_h = node.rect.height().max(1.0);
                        // Use the authored placement extent as-is for
                        // every connector class — that is the size MSL
                        // authors intended (Flange_a's 20×20 dot, the
                        // 20×20 RealInput triangle on plain blocks, the
                        // 40×40 RealInput on LimPID). OMEdit / Dymola
                        // render at this size; over-scaling produces a
                        // triangle that dominates the icon body.
                        let half_x = (port_size_x_icon * 0.5 / 100.0) * (parent_w * 0.5);
                        let half_y = (port_size_y_icon * 0.5 / 100.0) * (parent_h * 0.5);
                        let world_rect = lunco_canvas::Rect::from_min_max(
                            lunco_canvas::Pos::new(world.x - half_x, world.y - half_y),
                            lunco_canvas::Pos::new(world.x + half_x, world.y + half_y),
                        );
                        let s_rect = ctx.viewport.world_rect_to_screen(world_rect, ctx.screen_rect);
                        let port_rect = egui::Rect::from_min_max(
                            egui::pos2(s_rect.min.x, s_rect.min.y),
                            egui::pos2(s_rect.max.x, s_rect.max.y),
                        );
                        let palette = modelica_icon_palette_from_ctx(ctx.ui.ctx());
                        // Compose the connector icon's orientation from
                        // (a) the parent's mirror flags so a mirrored
                        // parent (e.g. `extent={{22,-50},{2,-30}}` on
                        // speedSensor) flips the connector icon too —
                        // RealOutput's TIP must point AWAY from the
                        // parent regardless of which canvas side it
                        // ends up on, and (b) the port's authored
                        // `Placement(transformation(rotation=...))` so
                        // a `rotation=270` input sits with its
                        // triangle pointing the right way (e.g. PI's
                        // `u_m` on the bottom edge points up).
                        // MLS `rotation=270` on a port placement means
                        // 270° CCW *in the visual frame* (where Y is
                        // down, i.e. screen frame) — rotation=270 on
                        // PI's `u_m` produces a triangle pointing UP
                        // on screen. Our `to_screen` applies rotation
                        // in Modelica's +Y-up frame and then flips Y,
                        // which is equivalent to rotating CW in the
                        // visual frame. Negate so the visual outcome
                        // matches MLS / OMEdit.
                        // Include the PARENT's rotation in the port
                        // marker's orientation. Without this, when
                        // the parent is rotated (e.g. addSat at
                        // rotation=270), only the port POSITION is
                        // rotated — the connector arrow keeps its
                        // default orientation and ends up pointing
                        // the wrong way relative to the rotated
                        // icon. Adding the parent's rotation makes
                        // the marker rotate WITH the icon body so
                        // the arrow tip always points into the icon.
                        let port_orientation = crate::icon_paint::IconOrientation {
                            rotation_deg: self.rotation_deg - port_rotation_deg,
                            mirror_x: self.mirror_x,
                            mirror_y: self.mirror_y,
                        };
                        crate::icon_paint::paint_graphics_themed(
                            painter,
                            port_rect,
                            icon.coordinate_system,
                            port_orientation,
                            None,
                            None,
                            palette.as_ref(),
                            &icon.graphics,
                        );
                        painted_authored = true;
                    }
            }

            if !painted_authored {
                // Generic fallback for unresolved connectors / classes
                // that ship no `Icon` annotation.
                let shape = match port.kind.as_str() {
                    "input" => PortShape::InputSquare,
                    "output" => PortShape::OutputTriangle,
                    _ => PortShape::AcausalCircle,
                };
                let fill = theme_snap.port_fill;
                let scale = (ctx.viewport.zoom / 3.0).sqrt().clamp(0.7, 1.4);
                let stroke = egui::Stroke::new(0.6 * scale, theme_snap.port_stroke);
                paint_port_shape(painter, center, shape, dir, fill, stroke, scale);
            }
        }

        // Hover tooltip. The canvas claims the whole widget rect
        // with `Sense::click_and_drag()` so `ui.interact(.., Sense::hover())`
        // and even `show_tooltip_at_pointer` get suppressed at the
        // visual's layer. Paint the tooltip card directly with the
        // foreground painter — bypasses egui's interaction layering
        // entirely.
        let cursor = ctx.ui.ctx().pointer_hover_pos();
        // Suppress the tooltip when the cursor isn't actually over
        // the canvas (e.g. floated past the widget edge while still
        // hovering the icon's *world rect*). Without this the card
        // can sit on top of the side panels because it paints in
        // an unclipped layer.
        let canvas_widget_rect = ctx.ui.max_rect();
        let in_canvas = cursor
            .map(|c| canvas_widget_rect.contains(c))
            .unwrap_or(false);
        let is_hovered = cursor
            .map(|c| rect.contains(c))
            .unwrap_or(false)
            && in_canvas;
        if is_hovered && !self.instance_name.is_empty() {
            let cursor = cursor.unwrap();
            let snap =
                lunco_viz::kinds::canvas_plot_node::fetch_node_state(
                    ctx.ui.ctx(),
                );
            let prefix = format!("{}.", self.instance_name);
            let mut rows: Vec<(&String, &f64)> = snap
                .values
                .iter()
                .filter(|(k, _)| k.starts_with(&prefix))
                .collect();
            rows.sort_by(|a, b| a.0.cmp(b.0));
            paint_hover_card(
                ctx.ui,
                cursor,
                &self.instance_name,
                &self.class_name,
                &rows,
            );
        }

        // Dashboard-style in-canvas control widget. Last call in
        // draw so the painter borrow taken above has ended (Rust
        // NLL allows ui to be reborrowed mutably here for
        // `ui.interact`). The widget is always visible while the
        // icon is rendered and captures pointer events itself so
        // dragging the slider does NOT also drag the node.
        paint_input_control_widget(ctx.ui, rect, &self.instance_name, ctx.viewport.zoom);
    }
    fn debug_name(&self) -> &str {
        "modelica.icon"
    }
}

/// Direct-paint hover card (foreground layer). Used because the
/// canvas's `Sense::click_and_drag()` swallows ordinary tooltip
/// hooks at the visual layer.
fn paint_hover_card(
    ui: &mut egui::Ui,
    cursor: egui::Pos2,
    instance: &str,
    class_name: &str,
    rows: &[(&String, &f64)],
) {
    let theme = lunco_canvas::theme::current(ui.ctx());
    let layer_id = egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new(("modelica_icon_hover_card", instance)),
    );
    let painter = ui.ctx().layer_painter(layer_id);
    // Clip to the canvas widget rect so the card never paints over
    // the side panels (the user would otherwise see a tooltip
    // ghost overlapping the Twin Browser when hovering an icon
    // near the canvas's left edge).
    let canvas_clip = ui.max_rect();
    let painter = painter.with_clip_rect(canvas_clip);

    // Build text lines first so we can size the card accordingly.
    let mut lines: Vec<(String, bool)> = Vec::with_capacity(rows.len() + 4);
    lines.push((instance.to_string(), true));
    if !class_name.is_empty() {
        lines.push((class_name.to_string(), false));
    }
    if rows.is_empty() {
        lines.push(("(no values yet — run a sim)".to_string(), false));
    } else {
        for (k, v) in rows {
            let short = k.strip_prefix(&format!("{instance}.")).unwrap_or(k);
            lines.push((format!("{short:<10}  {v:>10.4}"), false));
        }
    }

    let line_h = 14.0_f32;
    let pad = 6.0_f32;
    // Estimate width: 7 px per char (monospace). egui doesn't expose
    // `Painter::text_size` cheaply; this is plenty for the typical
    // path widths we render.
    let text_w = lines
        .iter()
        .map(|(s, _)| s.chars().count() as f32 * 7.0)
        .fold(0.0_f32, f32::max);
    let card_w = (text_w + pad * 2.0).clamp(120.0, 360.0);
    let card_h = lines.len() as f32 * line_h + pad * 2.0;

    // Anchor card to the right of the cursor with a small offset;
    // flip to the left if we'd run off the screen edge.
    let screen = ui.ctx().screen_rect();
    let mut origin =
        egui::pos2(cursor.x + 14.0, cursor.y + 14.0);
    if origin.x + card_w > screen.max.x {
        origin.x = cursor.x - card_w - 14.0;
    }
    if origin.y + card_h > screen.max.y {
        origin.y = cursor.y - card_h - 14.0;
    }
    let card_rect = egui::Rect::from_min_size(
        origin,
        egui::vec2(card_w, card_h),
    );
    // Drop shadow so the card pops over the diagram.
    painter.rect_filled(
        card_rect.translate(egui::vec2(0.0, 2.0)),
        6.0,
        theme.overlay_shadow,
    );
    painter.rect_filled(card_rect, 6.0, theme.overlay_fill);
    painter.rect_stroke(
        card_rect,
        6.0,
        egui::Stroke::new(1.0, theme.overlay_stroke),
        egui::StrokeKind::Outside,
    );

    let mut y = origin.y + pad;
    for (line, is_title) in &lines {
        let font = if *is_title {
            egui::FontId::proportional(13.0)
        } else {
            egui::FontId::monospace(11.0)
        };
        let color = if *is_title {
            theme.overlay_text
        } else {
            theme.overlay_text.gamma_multiply(0.85)
        };
        painter.text(
            egui::pos2(origin.x + pad, y),
            egui::Align2::LEFT_TOP,
            line,
            font,
            color,
        );
        y += line_h;
    }
}

/// Paint a chain of small bright dots along a polyline that march
/// from the first to the last vertex at constant screen-pixel speed.
/// Phase keyed off wall-clock `time` so all wires stay in sync.
/// Used as the "this connection is alive" overlay during simulation
/// — Simulink/SPICE-style, no per-edge flow data needed yet.
fn paint_flow_dots(
    painter: &egui::Painter,
    polyline: &[egui::Pos2],
    base_color: egui::Color32,
    time: f64,
    scale: f32,
) {
    if polyline.len() < 2 {
        return;
    }
    let mut total_len = 0.0_f32;
    for w in polyline.windows(2) {
        total_len += (w[1] - w[0]).length();
    }
    if total_len < 1.0 {
        return;
    }
    // Spacing + speed in screen pixels. Tuned iteratively: 64 px
    // looked empty; 28 px read as a dotted wire ("bumpy"); 32 px
    // was OK but still felt sparse on long runs; 22 px was better
    // but on short wire segments (a half-inch fluid line between
    // valve.port_b and engine.port) only 1–2 dots were ever
    // visible at one phase, so during the animation cycle the
    // wire spent most of its time looking static. 16 px gives
    // every short segment at least 3–4 dots in flight, so the
    // motion cue is always visible. Alpha stays moderate (180)
    // so the dots read as a moving stream rather than a solid
    // dotted line.
    const SPACING_PX: f32 = 16.0;
    const SPEED_PX_S: f32 = 36.0;
    let phase = ((time as f32) * SPEED_PX_S).rem_euclid(SPACING_PX);
    let dot_color = egui::Color32::from_rgba_unmultiplied(
        base_color.r(),
        base_color.g(),
        base_color.b(),
        180,
    );
    let mut s = phase;
    while s < total_len {
        // Walk the polyline to find the segment containing arc-length s.
        let mut acc = 0.0_f32;
        for w in polyline.windows(2) {
            let seg_len = (w[1] - w[0]).length();
            if s <= acc + seg_len {
                let t = ((s - acc) / seg_len).clamp(0.0, 1.0);
                let p = w[0] + (w[1] - w[0]) * t;
                // Slightly larger radius (was 2.2 × scale) so
                // the dot is unambiguous at low canvas zoom.
                painter.circle_filled(p, 2.6 * scale, dot_color);
                break;
            }
            acc += seg_len;
        }
        s += SPACING_PX;
    }
}

/// Dymola / OMEdit-style orthogonal edge: one horizontal-vertical-
/// horizontal Z-route with the bend at the x-midpoint. Collapses to
/// a straight segment when the endpoints are (near-)collinear on
/// either axis, avoiding degenerate zero-length jogs.
///
/// A richer routing pass (obstacle-avoidance, port-direction-aware
/// stubs, multiple-bend auto-layout) is a next step; this is the
/// pattern users already recognise.
/// Which edge of the icon a port sits on. Determines which axis the
/// wire's first segment ("stub") runs along — Dymola/OMEdit wire
/// pretty-routing convention. Modelica port placement is in (-100..100)
/// per axis; we classify by which extreme the port sits closest to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PortDir {
    Left,
    Right,
    Up,
    Down,
    /// Port sits in the interior of the icon (or no info). Routing
    /// degrades to plain Z-bend.
    #[default]
    None,
}

impl PortDir {
    fn as_str(self) -> &'static str {
        match self {
            PortDir::Left => "left",
            PortDir::Right => "right",
            PortDir::Up => "up",
            PortDir::Down => "down",
            PortDir::None => "",
        }
    }
    fn from_str(s: &str) -> PortDir {
        match s {
            "left" => PortDir::Left,
            "right" => PortDir::Right,
            "up" => PortDir::Up,
            "down" => PortDir::Down,
            _ => PortDir::None,
        }
    }
    /// Unit vector pointing *outward* from the icon at this edge,
    /// in screen coordinates (+Y down). Used to extend the wire
    /// stub away from the icon body.
    fn outward(self) -> (f32, f32) {
        match self {
            PortDir::Left => (-1.0, 0.0),
            PortDir::Right => (1.0, 0.0),
            PortDir::Up => (0.0, -1.0),
            PortDir::Down => (0.0, 1.0),
            PortDir::None => (0.0, 0.0),
        }
    }
}

// `rotate_modelica_point` / `rotate_local_point` / `mirror_local_point`
// retired — replaced by [`crate::icon_transform::IconTransform`], which
// folds mirror + rotate + scale + Y-flip into a single matrix that the
// projector applies via `apply` / `apply_dir`. See
// `crates/lunco-modelica/src/icon_transform.rs`.

/// Classify a 2D direction into one of the four cardinal icon edges,
/// in **screen frame** (+X right, +Y down — same convention as
/// [`PortDir::outward`]). Used to decide which way a wire stub
/// should extend out of a port.
///
/// The threshold makes any direction whose components are both close
/// to zero collapse to [`PortDir::None`] — Z-bend routing falls
/// through to the original midpoint logic in that case.
fn port_edge_dir(x: f32, y: f32) -> PortDir {
    let threshold = 0.4;
    let ax = x.abs();
    let ay = y.abs();
    if ax < threshold && ay < threshold {
        return PortDir::None;
    }
    if ax >= ay {
        if x >= 0.0 { PortDir::Right } else { PortDir::Left }
    } else if y >= 0.0 {
        // +Y down in screen → bottom edge of icon.
        PortDir::Down
    } else {
        PortDir::Up
    }
}

/// Map a Modelica connector type's leaf name to a wire colour.
///
/// Returns the **canonical MSL Icon line color** for that connector
/// kind — the same value the connector's authored `Icon(... lineColor=…)`
/// uses in the standard library. The diagram-level palette remap
/// (`Theme::modelica_icons`) re-tones these for the active theme on
/// the way to the painter, so dark-mode users get readable variants
/// while the underlying values match Dymola/OMEdit on a light
/// canvas. Used as the FALLBACK when we couldn't read the connector
/// instance's own `icon_color` from the AST.
fn wire_color_for(connector_type: &str) -> egui::Color32 {
    let leaf = connector_type
        .rsplit('.')
        .next()
        .unwrap_or(connector_type);
    use egui::Color32 as C;
    match leaf {
        // Electrical: red (positive) — MSL Pin uses {0,0,255}; OMEdit
        // renders these as solid red, but the canonical RGB is blue.
        // We follow the AST line color via icon_color when available.
        "Pin" | "PositivePin" | "NegativePin" | "Plug" | "PositivePlug"
        | "NegativePlug" => C::from_rgb(0, 0, 255),
        // Translational + rotational mechanics: BLACK — `Flange`
        // connectors author lineColor=black in MSL. OMEdit renders
        // mechanical wires black on the white canvas.
        "Flange_a" | "Flange_b" | "Flange" | "Support" => {
            C::from_rgb(0, 0, 0)
        }
        // Heat transfer: red (191,0,0) — canonical thermal color.
        "HeatPort_a" | "HeatPort_b" | "HeatPort" => C::from_rgb(191, 0, 0),
        // Fluid: blue (canonical Modelica.Fluid uses lineColor blue).
        "FluidPort" | "FluidPort_a" | "FluidPort_b" => C::from_rgb(0, 127, 255),
        // Real signals: deep blue {0,0,127} — what every MSL Real
        // signal connector authors as its lineColor. OMEdit renders
        // these as bold blue with arrowheads.
        "RealInput" | "RealOutput" => C::from_rgb(0, 0, 127),
        // Boolean signals: purple {255,0,255} per MSL Interfaces.
        "BooleanInput" | "BooleanOutput" => C::from_rgb(255, 0, 255),
        // Integer signals: green {255,127,0} (orange) per MSL.
        "IntegerInput" | "IntegerOutput" => C::from_rgb(255, 127, 0),
        // Frame_a/Frame_b (multibody): orange-brown.
        "Frame" | "Frame_a" | "Frame_b" => C::from_rgb(95, 95, 95),
        // Default — black (will remap to theme text on dark).
        _ => C::from_rgb(0, 0, 0),
    }
}

/// Per-edge wire visual. Carries the wire colour + the port-direction
/// hints baked in by the projector so each edge knows which axis to
/// extend before bending. Two stubs (one out of each port) followed
/// by a Z-bend gives the "wire grows out of the connector" look that
/// matches Dymola/OMEdit and reads much cleaner than the previous
/// always-x-midpoint Z.
struct OrthogonalEdgeVisual {
    color: egui::Color32,
    from_dir: PortDir,
    to_dir: PortDir,
    /// Authored / stored waypoints in **canvas world** coords
    /// (Modelica +Y is flipped to canvas +Y at projector time).
    /// When non-empty, the renderer emits a polyline through the
    /// waypoints instead of the auto Z-bend.
    waypoints_world: Vec<CanvasPos>,
    /// True when the connection is causal (output→input signal),
    /// so the renderer emits an arrowhead at the input end. False
    /// for acausal connectors (Pin, Flange, FluidPort, …) — the
    /// MLS convention is symmetric arrows-or-no-arrows for those.
    is_causal: bool,
    /// Fully-qualified source port path, e.g. `"engine.thrust"` /
    /// `"tank.fuel_out"`. Used to look up the current value (and
    /// its unit) for the hover tooltip, and for flow-animation
    /// direction (sign of `{source_path}.{flow_var}` at runtime).
    source_path: String,
    /// Fully-qualified target port path. Secondary — used only when
    /// the source path has no sampled value (e.g. inputs are only
    /// visible on the target side).
    target_path: String,
    /// Connector causality classification (Input / Output / Acausal)
    /// derived from the connector class AST at projection time.
    /// Drives arrowhead rendering and animation eligibility.
    kind: crate::visual_diagram::PortKind,
    /// Flow variables declared on the connector class (name + unit).
    /// Empty for causal signals — those never animate. Non-empty →
    /// we sample `{source_path}.{name}` to drive flow animation and
    /// to populate the hover tooltip with each variable + unit.
    flow_vars: Vec<crate::visual_diagram::FlowVarMeta>,
    /// Short class-name for the tooltip label when the connector
    /// class has no description string. Matches the MSL-style
    /// "what is this wire carrying" intuition (e.g. `"FuelPort_a"`).
    connector_leaf: String,
}

impl Default for OrthogonalEdgeVisual {
    fn default() -> Self {
        Self {
            color: wire_color_for(""),
            from_dir: PortDir::None,
            to_dir: PortDir::None,
            waypoints_world: Vec::new(),
            is_causal: false,
            source_path: String::new(),
            target_path: String::new(),
            kind: crate::visual_diagram::PortKind::Acausal,
            flow_vars: Vec::new(),
            connector_leaf: String::new(),
        }
    }
}

/// Stub length in screen pixels — long enough to clear the port dot
/// (which is itself ~4 px) and read clearly as "the wire exits the
/// port" at typical zoom levels, while still leaving room for the
/// Z-bend body. Earlier values around 10 px disappeared on default
/// auto-fit zoom; 18 stays readable across normal zoom range.
const STUB_PX: f32 = 18.0;

impl EdgeVisual for OrthogonalEdgeVisual {
    fn draw(
        &self,
        ctx: &mut DrawCtx,
        from: CanvasPos,
        to: CanvasPos,
        selected: bool,
    ) {
        // Apply the active modelica icon palette so the canonical MSL
        // wire colors (deep blue for signals, black for mechanical,
        // red for thermal, etc.) get re-toned for dark themes via the
        // same anchor table the icon primitives go through. Light
        // theme = identity, so OMEdit-style colors come through
        // unchanged.
        let palette = modelica_icon_palette_from_ctx(ctx.ui.ctx());
        let mapped = palette
            .as_ref()
            .map(|p| p.remap(self.color))
            .unwrap_or(self.color);
        let col = if selected {
            // Selection: brighten the per-type colour rather than
            // collapsing every wire to one universal "selected" blue.
            // Keeps the connector type recognisable through selection
            // chrome.
            brighten(mapped)
        } else {
            mapped
        };
        // OMEdit/Dymola convention: causal signal wires carry the
        // visual weight (they're the "logical flow" the reader cares
        // about) and render thicker than acausal physical wires.
        // Mechanical / thermal / electrical chains stay at the slim
        // default so dense plant networks don't overwhelm the eye.
        //
        // OMEdit / Dymola use roughly fixed pixel widths for wires
        // regardless of zoom — wires read as 1-2px hairlines at any
        // magnification. We mirror that: stroke width is a constant
        // pixel value, *not* multiplied by viewport.zoom (which is
        // pixels-per-world-unit and ranges ~0.5..10 across normal
        // fit/zoom-in extremes; multiplying by it produced 10px
        // wires at typical fit zoom). Arrows + port markers still
        // scale gently below.
        let base_width = if selected {
            if self.is_causal { 2.2 } else { 1.7 }
        } else if self.is_causal {
            1.6
        } else {
            1.1
        };
        let zoom_norm = (ctx.viewport.zoom / 3.0).sqrt().clamp(0.7, 1.6);
        let width = base_width * zoom_norm;
        // `scale` is exposed downstream for arrows / stubs / port
        // markers — same gently-damped formula keeps them in
        // sensible proportion to the wires.
        let scale = zoom_norm;
        let width = base_width * scale;
        let stroke = egui::Stroke::new(width, col);
        let painter = ctx.ui.painter();

        // Authored polyline: if the edge carries waypoints (from a
        // `connect(...) annotation(Line(points={{x,y},...}))` clause
        // or a user edit), emit a stub-from-port → waypoints → stub-
        // into-port polyline and skip the auto-Z router entirely.
        //
        // Optimistic fallback during drag: the waypoints are baked in
        // canvas-world coords at projection time. If the user is mid-
        // drag of one of the connected nodes, the port has moved but
        // the waypoints haven't, so the strict polyline form draws
        // an obvious zigzag back to the stale anchor. Detect that
        // (port noticeably far from the nearest authored endpoint)
        // and fall through to auto-Z so the wire visually tracks the
        // dragged node — when re-projection lands the waypoints come
        // back into alignment.
        if !self.waypoints_world.is_empty() {
            let from_screen = egui::pos2(from.x, from.y);
            let to_screen = egui::pos2(to.x, to.y);
            let way_screen: Vec<egui::Pos2> = self
                .waypoints_world
                .iter()
                .map(|p| {
                    let s = ctx
                        .viewport
                        .world_to_screen(*p, ctx.screen_rect);
                    egui::pos2(s.x, s.y)
                })
                .collect();
            // Stale-anchor guard: a fresh A* polyline is *always*
            // strictly orthogonal — its first and last segments share
            // an axis with their respective ports (the bend-only
            // emitter forces this). If either is off-axis the port
            // has moved relative to its waypoint anchor, i.e. the
            // user is dragging the node and re-projection hasn't
            // landed yet. Off-axis-ness, not distance, is the right
            // signal: legit long L-stubs stay axis-aligned at any
            // zoom, while a 1-px drag offset already breaks alignment.
            const ALIGN_TOL: f32 = 1.0;
            let first_far = way_screen
                .first()
                .map(|p| {
                    (p.x - from_screen.x).abs() > ALIGN_TOL
                        && (p.y - from_screen.y).abs() > ALIGN_TOL
                })
                .unwrap_or(false);
            let last_far = way_screen
                .last()
                .map(|p| {
                    (p.x - to_screen.x).abs() > ALIGN_TOL
                        && (p.y - to_screen.y).abs() > ALIGN_TOL
                })
                .unwrap_or(false);
            if !(first_far || last_far) {
                let mut pts = Vec::with_capacity(way_screen.len() + 2);
                pts.push(from_screen);
                pts.extend(way_screen.iter().copied());
                pts.push(to_screen);
                for w in pts.windows(2) {
                    painter.line_segment([w[0], w[1]], stroke);
                }
                // No wire-end arrowhead for causal signals: the
                // destination connector class's authored `Icon`
                // (RealInput's filled triangle, BooleanInput's, …)
                // already paints the visible arrow at the port
                // location. Drawing a `paint_arrowhead` on top
                // produced a tiny duplicate triangle overlapping the
                // bigger authored icon — visually muddy and
                // off-spec relative to OMEdit / Dymola. The icon
                // renderer in [`IconNodeVisual::draw`] handles it.
                return;
            }
            // else: fall through to auto-Z below
        }

        // Build an orthogonal polyline using port-direction-aware
        // elbow placement. See [`route_orthogonal`] for the full case
        // analysis (parallel-aligned, parallel-opposed, perpendicular,
        // unknown). This replaces the older "always-midpoint Z" router
        // that produced wires crossing through icon bodies whenever
        // a port faced away from its peer.
        let polyline = route_orthogonal(
            egui::pos2(from.x, from.y),
            self.from_dir,
            egui::pos2(to.x, to.y),
            self.to_dir,
            STUB_PX * scale,
        );
        for w in polyline.windows(2) {
            painter.line_segment([w[0], w[1]], stroke);
        }

        // No wire-end arrowhead — see the authored-waypoint branch
        // above for the rationale (the destination connector class's
        // authored Icon is the arrow). Kept the helper available
        // (`paint_arrowhead`) for the orthogonal router's dev visual
        // / future non-MSL connection kinds, but the default MSL
        // signal path no longer calls it.
        if false && self.is_causal && polyline.len() >= 2 {
            let n = polyline.len();
            paint_arrowhead(
                painter,
                polyline[n - 2],
                polyline[n - 1],
                col,
                scale,
            );
        }

        // Live-flow animation: small dots moving along the polyline
        // at constant speed. Skips when no signal data is present
        // (sim never ran / paused) so a static diagram doesn't pulse
        // for no reason. Real per-edge flow magnitude/direction is a
        // follow-up — for now this just signals "this connection is
        // live."
        // Flow-dot animation is a *live status indicator* — it only
        // runs when the simulator is actively stepping AND a flow
        // variable has a non-negligible magnitude. Paused state → no
        // dots, even if the last sampled m_dot was large. No flow
        // variable on this connector (causal signals: throttle,
        // thrust, mass) → never animated.
        // Flow animation: the monotonic `anim_time` only advances
        // while the simulator is stepping, so pausing freezes the
        // dots in place (visible but still). Unpausing resumes from
        // the same phase — no teleport, no fresh cycle.
        let anim_time = ctx
            .ui
            .ctx()
            .data(|d| {
                d.get_temp::<f64>(egui::Id::new("lunco_modelica_flow_anim_time"))
            })
            .unwrap_or(0.0);
        let node_state =
            lunco_viz::kinds::canvas_plot_node::fetch_node_state(ctx.ui.ctx());
        const ACTIVITY_EPS: f64 = 1e-6;
        // Decide animation direction relative to the polyline's
        // visual src→tgt orientation. Returns Some(physical_flow_v)
        // where positive ⇒ flow goes src→tgt (paint as-is) and
        // negative ⇒ flow goes tgt→src (reverse polyline).
        let physical_flow = if let Some(fv) = self.flow_vars.first() {
            // Acausal flow connector. MLS §9.3.1 convention:
            // `port.m_flow > 0` ⇒ mass enters the component THROUGH
            // that port (i.e. fluid flows tgt→src in our visual). So
            // `physical_src_to_tgt = -src.m_flow`. We sample the
            // source side first; if the compiler eliminated it (one
            // side of an a+b=0 pair often is), fall back to the
            // target side and re-flip the sign accordingly.
            let src_key = format!("{}.{}", self.source_path, fv.name);
            let tgt_key = format!("{}.{}", self.target_path, fv.name);
            if let Some(&v_src) = node_state.values.get(&src_key) {
                Some(-v_src)
            } else {
                node_state.values.get(&tgt_key).copied()
            }
        } else {
            // Causal signal: value lives on the source side; the
            // visual direction src→tgt already matches the data flow
            // (output → input), so a positive sample reads as
            // forward, negative as reverse. We treat the magnitude
            // as activity and pin direction to the polyline because
            // a causal signal "going negative" doesn't reverse the
            // wire — a controller output of -1.0 still flows from
            // the output to the input it's wired to.
            node_state
                .values
                .get(&self.source_path)
                .or_else(|| node_state.values.get(&self.target_path))
                .map(|&v| v.abs())
        };
        if let Some(v) = physical_flow {
            if v.abs() > ACTIVITY_EPS {
                if v < 0.0 {
                    let mut rev = polyline.clone();
                    rev.reverse();
                    paint_flow_dots(painter, &rev, col, anim_time, scale);
                } else {
                    paint_flow_dots(painter, &polyline, col, anim_time, scale);
                }
            }
        }

        // Hover tooltip — "<label>: <value> <unit>" when the pointer
        // is within HOVER_PX of any segment. Value is sampled from
        // the per-frame NodeStateSnapshot; renders nothing when
        // there's no sim (tooltip still shows the label + "n/a").
        if let Some(p) = ctx.ui.ctx().pointer_hover_pos() {
            const HOVER_PX: f32 = 8.0;
            let hit = polyline
                .windows(2)
                .any(|w| dist_point_to_segment(p, w[0], w[1]) <= HOVER_PX);
            if hit {
                let state = lunco_viz::kinds::canvas_plot_node::fetch_node_state(
                    ctx.ui.ctx(),
                );
                let text = edge_hover_text(self, &state);
                paint_wire_tooltip(painter, p, &text, col);
            }
        }
    }

    /// Hit-test the simplified path. Cheap enough to do at full
    /// fidelity on every click; refining for stubs would add cost
    /// but no detectability benefit (stubs are 10px each).
    fn hit(
        &self,
        world_pos: lunco_canvas::Pos,
        from_world: lunco_canvas::Pos,
        to_world: lunco_canvas::Pos,
    ) -> bool {
        let threshold_sq = 16.0_f32;
        let dx = to_world.x - from_world.x;
        let dy = to_world.y - from_world.y;
        if dx.abs() < 1.0 || dy.abs() < 1.0 {
            return segment_dist_sq(world_pos, from_world, to_world) <= threshold_sq;
        }
        let midx = from_world.x + dx * 0.5;
        let p0 = from_world;
        let p1 = lunco_canvas::Pos::new(midx, from_world.y);
        let p2 = lunco_canvas::Pos::new(midx, to_world.y);
        let p3 = to_world;
        segment_dist_sq(world_pos, p0, p1) <= threshold_sq
            || segment_dist_sq(world_pos, p1, p2) <= threshold_sq
            || segment_dist_sq(world_pos, p2, p3) <= threshold_sq
    }
}

/// Translate `p` by `len` pixels in `dir`'s outward direction.
fn step(p: CanvasPos, dir: PortDir, len: f32) -> CanvasPos {
    let (ux, uy) = dir.outward();
    CanvasPos::new(p.x + ux * len, p.y + uy * len)
}

/// Compute an orthogonal polyline routed between two ports, in
/// **screen coords** (+Y down). The router emits a stub from each
/// port in its outward direction, then connects the stub-ends with
/// either an L-elbow (perpendicular ports) or a Z-bend (parallel /
/// unknown), choosing pivot positions that keep the wire from
/// doubling back across the icon body.
///
/// Cases (where `f`/`t` are the port-side stub endpoints):
///
/// * **Perpendicular** (one horizontal, one vertical): single
///   L-elbow at the corner aligned with each port's exit axis. No
///   Z-bend needed.
///
/// * **Parallel, opposed** (e.g. Right→Left, both helping): classic
///   Z-bend pivoted at the midpoint along the ports' shared exit
///   axis. Stubs already pointed at each other so the elbow lives
///   between them.
///
/// * **Parallel, same direction** (e.g. both Right) or **port faces
///   away from peer**: the "helping" extent is pushed past the
///   farther port + STUB so the wire wraps around instead of
///   doubling back through the source icon. Produces a U-shape when
///   both ports face the same direction.
///
/// * **One unknown direction**: defer to the known port's axis;
///   midpoint Z. Both unknown: plain horizontal-first Z.
///
/// Output always starts at `from` and ends at `to`; intermediate
/// points are inserted only when needed (no zero-length segments).
fn route_orthogonal(
    from: egui::Pos2,
    from_dir: PortDir,
    to: egui::Pos2,
    to_dir: PortDir,
    stub: f32,
) -> Vec<egui::Pos2> {
    use PortDir::*;
    let f_horiz = matches!(from_dir, Left | Right);
    let f_vert = matches!(from_dir, Up | Down);
    let t_horiz = matches!(to_dir, Left | Right);
    let t_vert = matches!(to_dir, Up | Down);

    // Stub-ends — extend each port outward by `stub` even when the
    // direction "doesn't help", so the wire is visibly attached to
    // the connector and the elbow logic below has a fixed anchor.
    let (uxf, uyf) = from_dir.outward();
    let (uxt, uyt) = to_dir.outward();
    let f_stub = if from_dir == None {
        from
    } else {
        egui::pos2(from.x + uxf * stub, from.y + uyf * stub)
    };
    let t_stub = if to_dir == None {
        to
    } else {
        egui::pos2(to.x + uxt * stub, to.y + uyt * stub)
    };

    // "Helps" = the port's outward axis carries us toward the other
    // port. When false, the elbow has to wrap around the icon to
    // avoid crossing through it.
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let from_helps = uxf * dx + uyf * dy > 0.0;
    let to_helps = uxt * (-dx) + uyt * (-dy) > 0.0;

    let mut pts: Vec<egui::Pos2> = Vec::with_capacity(6);
    pts.push(from);

    // Decide the inner routing between f_stub and t_stub.
    if (f_horiz && t_vert) || (f_vert && t_horiz) {
        // Perpendicular ports → clean two-segment L with the corner
        // at the intersection of each port's exit axis. *No stubs*
        // here: the parallel case below uses stubs to make sure the
        // wire exits the icon body before bending, but on a
        // perpendicular pair an unconditionally-added stub causes a
        // visible back-and-forth jog when the stub overshoots the
        // corner along its axis (Tank.Down + Valve.Left at almost-
        // aligned y was the canonical failure: 10 px down, 6 px back
        // up to the corner, then right). Going straight to the
        // corner produces the OMEdit/Dymola-style elbow users
        // expect.
        let corner = if f_horiz {
            egui::pos2(to.x, from.y)
        } else {
            egui::pos2(from.x, to.y)
        };
        if corner != from && corner != to {
            pts.push(corner);
        }
        pts.push(to);
        pts.dedup_by(|a, b| (a.x - b.x).abs() < 0.5 && (a.y - b.y).abs() < 0.5);
        return pts;
    }
    if from_dir != None {
        pts.push(f_stub);
    }

    if f_horiz && t_horiz {
        // Both horizontal. Pivot Y at midway between stub-ends;
        // pivot X at midway between stub-ends if both helping
        // (classic Z), else push past the trailing port + stub
        // so the wire wraps around instead of crossing the icon.
        let pivot_x = if from_helps && to_helps {
            (f_stub.x + t_stub.x) * 0.5
        } else if !from_helps {
            // from-stub points the wrong way — push pivot past
            // from-stub in its outward direction.
            f_stub.x
        } else {
            t_stub.x
        };
        let pivot_y = (f_stub.y + t_stub.y) * 0.5;
        pts.push(egui::pos2(pivot_x, f_stub.y));
        if (pivot_y - f_stub.y).abs() > 0.5 {
            pts.push(egui::pos2(pivot_x, pivot_y));
            pts.push(egui::pos2(t_stub.x, pivot_y));
        } else {
            pts.push(egui::pos2(t_stub.x, f_stub.y));
        }
    } else if f_vert && t_vert {
        // Mirror of the both-horizontal case.
        let pivot_y = if from_helps && to_helps {
            (f_stub.y + t_stub.y) * 0.5
        } else if !from_helps {
            f_stub.y
        } else {
            t_stub.y
        };
        let pivot_x = (f_stub.x + t_stub.x) * 0.5;
        pts.push(egui::pos2(f_stub.x, pivot_y));
        if (pivot_x - f_stub.x).abs() > 0.5 {
            pts.push(egui::pos2(pivot_x, pivot_y));
            pts.push(egui::pos2(pivot_x, t_stub.y));
        } else {
            pts.push(egui::pos2(f_stub.x, t_stub.y));
        }
    } else {
        // At least one direction unknown. Defer to whichever side
        // has a known direction; if both unknown, pick horizontal-
        // first Z-bend.
        let horizontal_first = f_horiz || t_horiz || (!f_vert && !t_vert);
        if horizontal_first {
            let midx = (f_stub.x + t_stub.x) * 0.5;
            pts.push(egui::pos2(midx, f_stub.y));
            pts.push(egui::pos2(midx, t_stub.y));
        } else {
            let midy = (f_stub.y + t_stub.y) * 0.5;
            pts.push(egui::pos2(f_stub.x, midy));
            pts.push(egui::pos2(t_stub.x, midy));
        }
    }

    if to_dir != None {
        pts.push(t_stub);
    }
    pts.push(to);

    // De-dup adjacent identical points (collinear cases above can
    // produce degenerate runs); a polyline with zero-length segments
    // confuses both the renderer and the flow-dot animator.
    pts.dedup_by(|a, b| (a.x - b.x).abs() < 0.5 && (a.y - b.y).abs() < 0.5);
    pts
}

/// Serialise a [`PortKind`](crate::visual_diagram::PortKind) into the
/// short string used in edge JSON data, so the factory can round-trip
/// it without pulling in serde enum tagging.
fn port_kind_str(kind: crate::visual_diagram::PortKind) -> &'static str {
    match kind {
        crate::visual_diagram::PortKind::Input => "input",
        crate::visual_diagram::PortKind::Output => "output",
        crate::visual_diagram::PortKind::Acausal => "acausal",
    }
}

/// Build the wire hover tooltip text from AST-derived semantics —
/// header = connector class short-name; one line per declared flow
/// variable (name = value unit) for acausal connectors; otherwise
/// one line for the source-port value itself (causal signals).
/// Formats "n/a" for variables the sim hasn't sampled yet.
fn edge_hover_text(
    edge: &OrthogonalEdgeVisual,
    state: &lunco_viz::kinds::canvas_plot_node::NodeStateSnapshot,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = write!(&mut out, "{}", edge.connector_leaf);
    if edge.flow_vars.is_empty() {
        let v = state
            .values
            .get(&edge.source_path)
            .or_else(|| state.values.get(&edge.target_path))
            .copied();
        let value_str = match v {
            Some(v) => format!("{v:.3}"),
            None => "n/a".into(),
        };
        let _ = write!(&mut out, "\n  value = {value_str}");
    } else {
        for fv in &edge.flow_vars {
            let key = format!("{}.{}", edge.source_path, fv.name);
            let v = state.values.get(&key).copied();
            let value_str = match v {
                Some(v) => format!("{v:.3}"),
                None => "n/a".into(),
            };
            let unit = if fv.unit.is_empty() {
                String::new()
            } else {
                format!(" {}", fv.unit)
            };
            let _ = write!(&mut out, "\n  {} = {value_str}{unit}", fv.name);
        }
    }
    out
}

/// Perpendicular distance from point `p` to segment `a`→`b`, in
/// screen pixels. Used for hit-testing wire hover.
fn dist_point_to_segment(p: egui::Pos2, a: egui::Pos2, b: egui::Pos2) -> f32 {
    let ab = b - a;
    let ap = p - a;
    let len_sq = ab.x * ab.x + ab.y * ab.y;
    if len_sq < 1e-6 {
        return (p - a).length();
    }
    let t = ((ap.x * ab.x + ap.y * ab.y) / len_sq).clamp(0.0, 1.0);
    let proj = egui::pos2(a.x + ab.x * t, a.y + ab.y * t);
    (p - proj).length()
}

/// Paint a compact tooltip near `pointer` showing `text`. Uses the
/// wire's own color for the accent border so the user's eye links
/// the tooltip to the wire they're hovering.
fn paint_wire_tooltip(
    painter: &egui::Painter,
    pointer: egui::Pos2,
    text: &str,
    accent: egui::Color32,
) {
    // Draw on a Tooltip-order layer rather than the painter's own
    // layer so the tooltip sits ABOVE any node icons that might
    // overlap it. Wires are drawn before nodes (so ports sit on
    // top visually), which would otherwise occlude the edge
    // tooltip when the hover point is near a component body.
    let ctx = painter.ctx().clone();
    let top = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("lunco_modelica_wire_tooltip"),
    ));
    let font = egui::FontId::proportional(11.0);
    let galley = top.layout_no_wrap(
        text.to_string(),
        font,
        egui::Color32::from_rgb(235, 235, 240),
    );
    let pad = egui::vec2(6.0, 3.0);
    // Offset so the tooltip doesn't sit under the cursor.
    let min = egui::pos2(pointer.x + 12.0, pointer.y + 12.0);
    let rect = egui::Rect::from_min_size(min, galley.size() + pad * 2.0);
    top.rect_filled(rect, 3.0, egui::Color32::from_rgba_unmultiplied(20, 22, 28, 235));
    top.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, accent),
        egui::StrokeKind::Inside,
    );
    top.galley(rect.min + pad, galley, egui::Color32::PLACEHOLDER);
}

/// Paint a small filled triangle pointing from `tail` to `tip`.
/// Used to indicate signal direction at the input end of causal
/// connections — matches `arrow={Arrow.None,Arrow.Filled}` in MLS
/// `Line` annotations.
fn paint_arrowhead(
    painter: &egui::Painter,
    tail: egui::Pos2,
    tip: egui::Pos2,
    color: egui::Color32,
    scale: f32,
) {
    let dx = tip.x - tail.x;
    let dy = tip.y - tail.y;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (px, py) = (-uy, ux); // perpendicular
    // Base ~9×4.5 filled triangle to match OMEdit's heavier arrow
    // weight on signal connections. Clamp head_len to ≤40 % of the
    // final-segment length so the tail never reaches past the
    // previous waypoint (orthogonal routing leaves a small stub at
    // the input port; an oversized head would eat the whole stub).
    let head_len: f32 = (9.0 * scale).min(len * 0.4);
    let head_halfw: f32 = head_len * 0.5;
    let base = egui::pos2(tip.x - ux * head_len, tip.y - uy * head_len);
    let b1 = egui::pos2(base.x + px * head_halfw, base.y + py * head_halfw);
    let b2 = egui::pos2(base.x - px * head_halfw, base.y - py * head_halfw);
    painter.add(egui::Shape::convex_polygon(
        vec![tip, b1, b2],
        color,
        egui::Stroke::NONE,
    ));
}

/// Render Dashboard-style in-canvas control widgets for every
/// bounded input attached to this instance. Mirrors the Simulink
/// `Dashboard.Slider` / SCADA HMI pattern: small interactive strips
/// rendered ON the node, separate from the icon body, that capture
/// pointer events directly so dragging a slider doesn't also drag
/// the node.
///
/// Coverage:
/// - One vertical strip per input whose key starts with `<instance>.`
///   *and* has finite declared `min`/`max` bounds.
/// - Strips are stacked side by side along the right edge of the
///   icon, leaf-name tooltip on hover so users learn which strip
///   controls which input.
/// - Inputs without bounds are skipped (we'd have nothing to map
///   drag distance against). A future revision can add a
///   knob-style relative-drag widget for unbounded inputs and an
///   explicit `__LunCo_inputControl(target=...)` annotation for
///   model authors who want fine control over placement / kind.
fn paint_input_control_widget(
    ui: &mut egui::Ui,
    icon_rect: egui::Rect,
    instance_name: &str,
    zoom: f32,
) {
    if instance_name.is_empty() || icon_rect.height() < 24.0 {
        return;
    }
    let snap = lunco_viz::kinds::canvas_plot_node::fetch_input_control_snapshot(ui.ctx());
    let prefix = format!("{instance_name}.");

    // Collect all bounded inputs for this instance, sorted by name
    // for stable left-to-right order across frames (the snapshot's
    // HashMap iteration is non-deterministic).
    let mut bound: Vec<(String, f64, f64, f64)> = snap
        .inputs
        .iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .filter_map(|(name, (value, min, max))| {
            let (mn, mx) = (min.as_ref()?, max.as_ref()?);
            if mx <= mn {
                return None;
            }
            Some((name.clone(), *value, *mn, *mx))
        })
        .collect();
    if bound.is_empty() {
        return;
    }
    bound.sort_by(|a, b| a.0.cmp(&b.0));

    // Strip geometry sized as a fraction of the icon's rendered
    // height — keeps the slider visually proportional to the icon
    // at every canvas zoom (the previous zoom-clamped formula
    // floored at 0.4× / capped at 2×, so on a large diagram the
    // slider looked tiny next to the icon body). 8 % of icon
    // height is the same ratio Dymola uses for inline indicators.
    // Floor at a few pixels so the strip stays grabbable even
    // when the icon is heavily zoomed out.
    let strip_width = (icon_rect.height() * 0.08).max(4.0);
    let strip_gap = strip_width * 0.4;
    let strip_pad = strip_width * 0.5;
    let h = icon_rect.height() * 0.7;
    let s = strip_width / 10.0;
    let strip_top_y = icon_rect.center().y - h * 0.5;

    for (idx, (name, value, mn, mx)) in bound.iter().enumerate() {
        let x = icon_rect.right()
            - strip_pad
            - strip_width
            - (idx as f32) * (strip_width + strip_gap);
        let strip_rect = egui::Rect::from_min_size(
            egui::pos2(x, strip_top_y),
            egui::vec2(strip_width, h),
        );

        // Publish the strip's screen-rect so the canvas's raw-input
        // dispatch skips node-drag when the pointer is inside. This
        // is how the slider "wins" the pointer without competing
        // with the canvas's tool interaction — see
        // [`lunco_canvas::canvas::push_canvas_widget_rect`].
        lunco_canvas::canvas::push_canvas_widget_rect(ui.ctx(), strip_rect);

        // Background trough — desaturated near-black so the fill
        // pops without fighting the icon body underneath. Higher
        // corner radius (was 3) reads as a softer control surface,
        // not a tooltip rectangle.
        let trough_color = egui::Color32::from_rgba_unmultiplied(28, 30, 38, 220);
        let radius = (strip_width * 0.45).min(5.0);
        ui.painter().rect_filled(strip_rect, radius, trough_color);

        // Filled portion = current value normalised against bounds.
        let frac = ((*value - *mn) / (*mx - *mn)).clamp(0.0, 1.0) as f32;
        if frac > 0.0 {
            let fill_h = strip_rect.height() * frac;
            let fill_rect = egui::Rect::from_min_size(
                egui::pos2(strip_rect.min.x, strip_rect.max.y - fill_h),
                egui::vec2(strip_rect.width(), fill_h),
            );
            // Theme accent — same blue the rest of the workbench
            // uses for "active / live signal" state. Slightly more
            // saturated than the previous flat blue so it reads as
            // a deliberate control rather than a flat fill.
            let fill_color = egui::Color32::from_rgb(70, 160, 240);
            ui.painter().rect_filled(fill_rect, radius, fill_color);
            // Indicator line at the fill top — tells the eye
            // exactly where the current value sits without having
            // to estimate from the gradient. 1.5 px stays crisp at
            // every zoom level the strip is visible at.
            let y = strip_rect.max.y - fill_h;
            ui.painter().line_segment(
                [egui::pos2(strip_rect.min.x, y), egui::pos2(strip_rect.max.x, y)],
                egui::Stroke::new(1.5 * s, egui::Color32::from_rgb(220, 235, 250)),
            );
        }

        // Outline so the strip stays visible against any icon body.
        // Slightly dimmer than before — the indicator line plus the
        // deeper trough already give the control its silhouette.
        ui.painter().rect_stroke(
            strip_rect,
            radius,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 130, 145)),
            egui::StrokeKind::Inside,
        );

        // `ui.interact` at the strip's rect. Seeding the Id with
        // the input's fully-qualified name so two instances of the
        // same component class (or two inputs on the same instance)
        // don't share drag state.
        let widget_id = egui::Id::new(("lunco_input_control", name.clone()));
        let response = ui.interact(strip_rect, widget_id, egui::Sense::click_and_drag());
        if response.dragged() || response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                // Vertical pointer position → value: top = max,
                // bottom = min (standard fader convention).
                let y_rel = (pos.y - strip_rect.min.y) / strip_rect.height();
                let inv = (1.0 - y_rel).clamp(0.0, 1.0) as f64;
                let new_value = mn + inv * (mx - mn);
                if (new_value - value).abs() > 1e-9 {
                    lunco_viz::kinds::canvas_plot_node::queue_input_write(
                        ui.ctx(),
                        name,
                        new_value,
                    );
                }
            }
        }
        if response.hovered() {
            let leaf = name.rsplit('.').next().unwrap_or(name);
            let tooltip = if (mx - mn - 100.0).abs() < 1e-6 && mn.abs() < 1e-6 {
                format!("{leaf}: {value:.1} %")
            } else {
                format!("{leaf}: {value:.3} (range {mn:.2} … {mx:.2})")
            };
            response.on_hover_text(tooltip);
        }
    }
}

/// Visual style of a port marker on a component icon. Mirrors the
/// OMEdit / Dymola convention so users can read connector causality
/// at a glance without hovering for the type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortShape {
    /// Filled square — `input` causality (RealInput, BooleanInput, …).
    InputSquare,
    /// Filled triangle pointing outward from the icon — `output`
    /// causality (RealOutput, BooleanOutput, …).
    OutputTriangle,
    /// Filled circle — acausal physical connectors (Pin, Flange, …).
    AcausalCircle,
}

/// Paint a port marker at `center` using the OMEdit shape convention
/// described on [`PortShape`]. `dir` orients the output triangle so
/// it points away from the icon body; ignored for square / circle.
fn paint_port_shape(
    painter: &egui::Painter,
    center: egui::Pos2,
    shape: PortShape,
    dir: PortDir,
    fill: egui::Color32,
    stroke: egui::Stroke,
    scale: f32,
) {
    // OMEdit shows the connector class's authored Icon (RealInput's
    // filled triangle, Flange's grey rectangle, RealOutput's outlined
    // triangle, …) — *not* a generic shape. The proper fix is to
    // render that Icon at each port location at icon-scale (TODO:
    // resolve `port.connector_type` → class → `extract_icon_inherited`
    // → `paint_graphics` into a port-sized rect). Until then this
    // helper draws a generic per-shape stand-in sized to roughly
    // match OMEdit's connector-icon visual weight so flange/input
    // markers are clearly visible.
    let r: f32 = 1.4 * scale;
    let R = r;
    match shape {
        PortShape::InputSquare => {
            let rect = egui::Rect::from_center_size(center, egui::vec2(R * 1.6, R * 1.6));
            painter.rect_filled(rect, 0.0, fill);
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
        }
        PortShape::OutputTriangle => {
            // Three-point triangle: tip in `dir`, base perpendicular.
            let (ox, oy) = dir.outward();
            // For PortDir::None fall back to a small square so the
            // port is still visible (no preferred orientation).
            if (ox, oy) == (0.0, 0.0) {
                let rect = egui::Rect::from_center_size(
                    center,
                    egui::vec2(R * 1.6, R * 1.6),
                );
                painter.rect_filled(rect, 0.0, fill);
                painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
                return;
            }
            // Perpendicular for the base: rotate (ox, oy) 90°.
            let (px, py) = (-oy, ox);
            let tip = egui::pos2(center.x + ox * R * 1.4, center.y + oy * R * 1.4);
            let b1 = egui::pos2(
                center.x - ox * R * 0.4 + px * R * 0.9,
                center.y - oy * R * 0.4 + py * R * 0.9,
            );
            let b2 = egui::pos2(
                center.x - ox * R * 0.4 - px * R * 0.9,
                center.y - oy * R * 0.4 - py * R * 0.9,
            );
            let pts = vec![tip, b1, b2];
            painter.add(egui::Shape::convex_polygon(pts.clone(), fill, stroke));
        }
        PortShape::AcausalCircle => {
            painter.circle_filled(center, R - 1.0, fill);
            painter.circle_stroke(center, R - 1.0, stroke);
        }
    }
}

/// Selection-state brightener — shifts each channel ~30% toward white
/// while preserving hue. Used so wires keep their domain colour even
/// while highlighted.
fn brighten(c: egui::Color32) -> egui::Color32 {
    let lift = |v: u8| (v as u16 + 80).min(255) as u8;
    egui::Color32::from_rgb(lift(c.r()), lift(c.g()), lift(c.b()))
}

/// Paint a dashed rectangle outline. Used for icon-only classes so
/// users see at a glance that the node is decorative (no
/// connectors). Dashes are expressed in screen pixels because the
/// caller has already transformed to screen-space — so the dash
/// pattern stays the same visual size regardless of zoom. `radius`
/// is currently unused (corners are sampled as-if straight for
/// simplicity); revisit if the corner elision gets noticed.
fn paint_dashed_rect(
    painter: &egui::Painter,
    rect: egui::Rect,
    _radius: f32,
    stroke: egui::Stroke,
) {
    let dash_len = 4.0;
    let gap_len = 3.0;
    let period = dash_len + gap_len;
    // Walk each of the four edges, emitting dash-sized segments.
    let edges = [
        (rect.min, egui::pos2(rect.max.x, rect.min.y)), // top
        (egui::pos2(rect.max.x, rect.min.y), rect.max), // right
        (rect.max, egui::pos2(rect.min.x, rect.max.y)), // bottom
        (egui::pos2(rect.min.x, rect.max.y), rect.min), // left
    ];
    for (a, b) in edges {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let len = (dx * dx + dy * dy).sqrt();
        if len < f32::EPSILON {
            continue;
        }
        let ux = dx / len;
        let uy = dy / len;
        let mut t = 0.0_f32;
        while t < len {
            let end = (t + dash_len).min(len);
            painter.line_segment(
                [
                    egui::pos2(a.x + ux * t, a.y + uy * t),
                    egui::pos2(a.x + ux * end, a.y + uy * end),
                ],
                stroke,
            );
            t += period;
        }
    }
}

/// Squared perpendicular distance from `p` to the finite segment
/// `(a,b)`. Endpoint-clamped — clicking past the end doesn't count.
fn segment_dist_sq(
    p: lunco_canvas::Pos,
    a: lunco_canvas::Pos,
    b: lunco_canvas::Pos,
) -> f32 {
    let ax = b.x - a.x;
    let ay = b.y - a.y;
    let len_sq = ax * ax + ay * ay;
    if len_sq < f32::EPSILON {
        let dx = p.x - a.x;
        let dy = p.y - a.y;
        return dx * dx + dy * dy;
    }
    let t = (((p.x - a.x) * ax + (p.y - a.y) * ay) / len_sq).clamp(0.0, 1.0);
    let foot_x = a.x + t * ax;
    let foot_y = a.y + t * ay;
    let dx = p.x - foot_x;
    let dy = p.y - foot_y;
    dx * dx + dy * dy
}

fn build_registry() -> VisualRegistry {
    let mut reg = VisualRegistry::new();
    // Generic in-canvas viz node kinds (plots today, dashboards /
    // cameras tomorrow). Lives in lunco-viz so it's reusable from any
    // domain plugin that wants embedded scopes — Modelica is just the
    // first integrator.
    lunco_viz::kinds::canvas_plot_node::register(&mut reg);
    crate::ui::text_node::register(&mut reg);
    reg.register_node_kind("modelica.icon", |data: &lunco_canvas::NodeData| {
        // Downcast to the typed payload the projector boxed (see
        // `IconNodeData`). Empty payload → render with defaults; the
        // visual handles a missing icon by showing the type label.
        let Some(d) = data.downcast_ref::<IconNodeData>() else {
            return IconNodeVisual::default();
        };
        let type_label = d
            .qualified_type
            .rsplit('.')
            .next()
            .unwrap_or(&d.qualified_type)
            .to_string();
        IconNodeVisual {
            type_label: type_label.clone(),
            class_name: type_label,
            icon_only: d.icon_only,
            expandable_connector: d.expandable_connector,
            icon_graphics: d.icon_graphics.clone(),
            diagram_graphics: d.diagram_graphics.clone(),
            parameters: d.parameters.clone(),
            rotation_deg: d.rotation_deg,
            mirror_x: d.mirror_x,
            mirror_y: d.mirror_y,
            instance_name: d.instance_name.clone(),
            port_connector_paths: d.port_connector_paths.clone(),
            is_conditional: d.is_conditional,
            parent_qualified_type: d.qualified_type.clone(),
        }
    });
    reg.register_edge_kind("modelica.connection", |data: &lunco_canvas::NodeData| {
        let Some(d) = data.downcast_ref::<ConnectionEdgeData>() else {
            return OrthogonalEdgeVisual::default();
        };
        let leaf = d
            .connector_type
            .rsplit('.')
            .next()
            .unwrap_or(&d.connector_type)
            .to_string();
        // PortKind from rumoca's typed classifier covers most cases,
        // but short-form connectors (`connector RealInput = input Real`)
        // currently land as `Acausal` because rumoca attaches the
        // `input`/`output` keyword to the alias-target, not to the
        // classifier's `class.causality` slot. Pattern-match the
        // canonical Modelica signal connector names so the wire still
        // renders as causal (thicker stroke + arrowhead at input).
        let causal_by_name = leaf.ends_with("Input") || leaf.ends_with("Output");
        let is_causal = matches!(
            d.kind,
            crate::visual_diagram::PortKind::Input | crate::visual_diagram::PortKind::Output,
        ) || causal_by_name;
        OrthogonalEdgeVisual {
            color: d
                .icon_color
                .unwrap_or_else(|| wire_color_for(&d.connector_type)),
            from_dir: d.from_dir,
            to_dir: d.to_dir,
            waypoints_world: d.waypoints_world.clone(),
            is_causal,
            source_path: d.source_path.clone(),
            target_path: d.target_path.clone(),
            kind: d.kind,
            flow_vars: d.flow_vars.clone(),
            connector_leaf: leaf,
        }
    });
    reg
}

// ─── Projection: VisualDiagram → lunco_canvas::Scene ────────────────

/// Modelica diagram coordinates are `(-100..100)` both axes with +Y
/// up. Width is a fixed 20×20 world-unit box — the typical
/// Modelica icon extent (`{{-10,-10},{10,10}}`). Dymola/OMEdit
/// render components at this size by default. Reading the actual
/// per-component `Icon` annotation extent is a follow-up.
const ICON_W: f32 = 20.0;
const ICON_H: f32 = 20.0;

/// Coordinate-system types + the two conversion functions between
/// them. Named wrappers around plain `(f32, f32)` so every place
/// the sign flip happens is explicit and typed — previously we had
/// ad-hoc `-y` negations scattered across the projector, the op
/// emitters, and the context-menu handler, and a missing negation
/// or a double-negation produced the hard-to-diagnose "position is
/// off" class of bugs.
///
/// Conventions:
///
/// - [`ModelicaPos`] — Modelica `.mo` source convention. +Y up.
///   Ranges typically `-100..100` per axis. This is the authored
///   coordinate that lands in `annotation(Placement(...))`.
///
/// - [`lunco_canvas::Pos`] — canvas world coordinate. +Y DOWN
///   (screen convention). This is what the canvas scene / viewport
///   consume and what hit-testing / rendering operates on.
///
/// The two differ only in the sign of Y. Keeping them as separate
/// types makes mis-conversion a type error instead of a silent off-
/// by-flip.
pub mod coords {
    use lunco_canvas::Pos as CanvasPos;

    /// Modelica-convention 2D point (+Y up).
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub struct ModelicaPos {
        pub x: f32,
        pub y: f32,
    }

    impl ModelicaPos {
        pub const fn new(x: f32, y: f32) -> Self {
            Self { x, y }
        }
    }

    /// Canvas world (+Y down) → Modelica (+Y up).
    #[inline]
    pub fn canvas_to_modelica(c: CanvasPos) -> ModelicaPos {
        ModelicaPos {
            x: c.x,
            y: -c.y,
        }
    }

    /// Modelica (+Y up) → canvas world (+Y down).
    #[inline]
    pub fn modelica_to_canvas(m: ModelicaPos) -> CanvasPos {
        CanvasPos::new(m.x, -m.y)
    }

    /// Canvas rect-min → Modelica centre. Used when committing a
    /// drag: the user's drag target lands as the icon's top-left in
    /// canvas coordinates, but Modelica placements are centre-
    /// anchored, so we shift by half the icon extent.
    #[inline]
    pub fn canvas_min_to_modelica_center(
        min: CanvasPos,
        icon_w: f32,
        icon_h: f32,
    ) -> ModelicaPos {
        canvas_to_modelica(CanvasPos::new(
            min.x + icon_w * 0.5,
            min.y + icon_h * 0.5,
        ))
    }
}

use coords::{canvas_to_modelica, ModelicaPos};

/// Fallback port layout when the component has no annotated port
/// positions. Alternates left / right edges at the vertical centre
/// for the first two ports (the common two-terminal shape), then
/// walks up both sides for any additional ports. Uses default icon
/// dimensions; for sized icons see [`port_fallback_offset_for_size`].
fn port_fallback_offset(index: usize, total: usize) -> (f32, f32) {
    port_fallback_offset_for_size(index, total, ICON_W, ICON_H)
}

/// Same fallback layout as [`port_fallback_offset`] but parameterised
/// by the icon's actual width/height — needed once Placement-driven
/// node sizing makes per-instance dimensions vary instead of always
/// being 20×20.
fn port_fallback_offset_for_size(
    index: usize,
    _total: usize,
    icon_w: f32,
    icon_h: f32,
) -> (f32, f32) {
    let side_left = index % 2 == 0;
    let row = index / 2; // 0 → middle, 1 → above, 2 → even higher
    let cy = icon_h * 0.5 - (row as f32) * (icon_h * 0.25);
    let cx = if side_left { 0.0 } else { icon_w };
    (cx, cy.clamp(0.0, icon_h))
}

/// Regex-scan `connect(a.b, c.d);` patterns in `source` and add
/// matching edges to `diagram`. Skips equations whose components
/// aren't in the diagram (missing nodes stay visually missing) or
/// that already exist as edges (keyed by unordered endpoint pair).
///
/// Deliberately permissive: doesn't validate port existence, doesn't
/// care about the line-continuation form, doesn't consult
/// annotations. "Text says A.x ↔ B.y; show a line between A and B".
fn recover_edges_from_source(source: &str, diagram: &mut VisualDiagram) {
    // Walk every class's `Equation::Connect` via rumoca AST and add
    // any edges the diagram-build path missed (typically connects on
    // top-level connectors that the component-graph builder skips
    // because they aren't sub-components). Iterating the AST gives
    // us proper handling of dotted port paths (`flange.phi`) without
    // a separate regex.
    let Ok(ast) = rumoca_phase_parse::parse_to_ast(source, "recover.mo") else {
        return;
    };

    // Build (instance_name → DiagramNodeId) index once per call.
    let index: HashMap<String, DiagramNodeId> = diagram
        .nodes
        .iter()
        .map(|n| (n.instance_name.clone(), n.id))
        .collect();

    // Track existing edges as unordered pairs so we don't double-
    // add when the AST path already caught a connection.
    let existing: std::collections::HashSet<((String, String), (String, String))> = diagram
        .edges
        .iter()
        .map(|e| {
            let a = (
                diagram
                    .get_node(e.source_node)
                    .map(|n| n.instance_name.clone())
                    .unwrap_or_default(),
                e.source_port.clone(),
            );
            let b = (
                diagram
                    .get_node(e.target_node)
                    .map(|n| n.instance_name.clone())
                    .unwrap_or_default(),
                e.target_port.clone(),
            );
            // Canonicalise to min/max so (A.x, B.y) == (B.y, A.x).
            if a <= b { (a, b) } else { (b, a) }
        })
        .collect();

    fn walk(
        class: &rumoca_session::parsing::ast::ClassDef,
        diagram: &mut VisualDiagram,
        index: &HashMap<String, DiagramNodeId>,
        existing: &std::collections::HashSet<((String, String), (String, String))>,
    ) {
        use rumoca_session::parsing::ast::Equation;
        for eq in &class.equations {
            let Equation::Connect { lhs, rhs, .. } = eq else { continue };
            // Only handle 2+ part references (`inst.port[.subport]`);
            // single-part bare-connector connects are caught by the
            // primary AST path that builds the component graph.
            let (src_comp, src_port) = match lhs.parts.as_slice() {
                [first, rest @ ..] if !rest.is_empty() => (
                    first.ident.text.to_string(),
                    rest.iter()
                        .map(|p| p.ident.text.as_ref())
                        .collect::<Vec<_>>()
                        .join("."),
                ),
                _ => continue,
            };
            let (tgt_comp, tgt_port) = match rhs.parts.as_slice() {
                [first, rest @ ..] if !rest.is_empty() => (
                    first.ident.text.to_string(),
                    rest.iter()
                        .map(|p| p.ident.text.as_ref())
                        .collect::<Vec<_>>()
                        .join("."),
                ),
                _ => continue,
            };
            let (Some(&src_id), Some(&tgt_id)) =
                (index.get(&src_comp), index.get(&tgt_comp))
            else {
                continue;
            };
            let pair = {
                let a = (src_comp.clone(), src_port.clone());
                let b = (tgt_comp.clone(), tgt_port.clone());
                if a <= b { (a, b) } else { (b, a) }
            };
            if existing.contains(&pair) {
                continue;
            }
            diagram.add_edge(src_id, src_port, tgt_id, tgt_port);
        }
        for nested in class.classes.values() {
            walk(nested, diagram, index, existing);
        }
    }
    for class in ast.classes.values() {
        walk(class, diagram, &index, &existing);
    }
}

fn project_scene(diagram: &VisualDiagram) -> (Scene, HashMap<DiagramNodeId, CanvasNodeId>) {
    let mut scene = Scene::new();
    let mut id_map: HashMap<DiagramNodeId, CanvasNodeId> = HashMap::new();

    for node in &diagram.nodes {
        let cid = scene.alloc_node_id();
        id_map.insert(node.id, cid);

        // Ports: map Modelica (-100..100, +Y up) to local icon box
        // (0..ICON_W, 0..ICON_H, +Y down). If a port has no
        // annotated position (both x and y at 0 — the default when
        // the component class didn't declare one), fall back to
        // distributing around the icon's edges: alternating left
        // and right for the classic two-terminal electrical shape,
        // extending up for more ports. Matches what OMEdit does
        // when Placement annotations are missing.
        // The single source of truth for this node's icon-local →
        // canvas-world transform. Built once by the importer from the
        // Placement, applied uniformly here for the rect, ports, and
        // (eventually) the icon body.
        let xform = node.icon_transform;

        // Bounding rect = AABB of the icon's local extent
        // ({{-100,-100},{100,100}} per MLS default) under the
        // transform. Honours rotation naturally (a 45°-rotated icon
        // gets a larger axis-aligned rect than its unrotated form).
        let ((min_wx, min_wy), (max_wx, max_wy)) =
            xform.local_aabb(-100.0, -100.0, 100.0, 100.0);
        let icon_w_local = (max_wx - min_wx).max(4.0);
        let icon_h_local = (max_wy - min_wy).max(4.0);

        let n_ports = node.component_def.ports.len();
        let ports: Vec<CanvasPort> = node
            .component_def
            .ports
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // Port positions in icon-local Modelica coords go
                // through the same transform — no per-feature
                // mirror/rotate branches, just one matrix multiply.
                // The result is in canvas world; we convert to
                // icon-local *screen* coords (relative to the rect's
                // top-left) since `CanvasPort.local_offset` is icon-
                // local, not world.
                let (wx, wy) = if p.x == 0.0 && p.y == 0.0 {
                    // Fallback layout: distribute around the rect.
                    // Already in icon-local screen coords — convert
                    // to world by adding the rect's top-left.
                    let (fx, fy) = port_fallback_offset_for_size(
                        i,
                        n_ports,
                        icon_w_local,
                        icon_h_local,
                    );
                    (min_wx + fx, min_wy + fy)
                } else {
                    xform.apply(p.x, p.y)
                };
                let lx = wx - min_wx;
                let ly = wy - min_wy;
                CanvasPort {
                    id: CanvasPortId::new(p.name.clone()),
                    local_offset: CanvasPos::new(lx, ly),
                    // AST-derived causality classification as a short
                    // string (`"input"` / `"output"` / `"acausal"`) —
                    // the canvas renderer's port-shape match reads
                    // this directly, so MSL naming conventions are
                    // no longer needed to pick the right shape.
                    kind: port_kind_str(p.kind).into(),
                }
            })
            .collect();

        scene.insert_node(CanvasNode {
            id: cid,
            rect: CanvasRect::from_min_size(
                CanvasPos::new(min_wx, min_wy),
                icon_w_local,
                icon_h_local,
            ),
            kind: "modelica.icon".into(),
            data: std::sync::Arc::new(IconNodeData {
                qualified_type: node.component_def.msl_path.clone(),
                icon_only: crate::ui::loaded_classes::is_icon_only_class(
                    &node.component_def.msl_path,
                ),
                expandable_connector: node.component_def.is_expandable_connector,
                icon_graphics: node.component_def.icon_graphics.clone(),
                diagram_graphics: if node.component_def.class_kind == "connector" {
                    node.component_def.diagram_graphics.clone()
                } else {
                    None
                },
                rotation_deg: node.icon_transform.rotation_deg,
                mirror_x: node.icon_transform.mirror_x,
                mirror_y: node.icon_transform.mirror_y,
                instance_name: node.instance_name.clone(),
                parameters: node
                    .component_def
                    .parameters
                    .iter()
                    .map(|p| {
                        let v = node
                            .parameter_values
                            .get(&p.name)
                            .cloned()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| p.default.clone());
                        let value = match si_unit_suffix(&p.param_type) {
                            Some(unit) if !v.is_empty() => format!("{v} {unit}"),
                            _ => v,
                        };
                        (p.name.clone(), value)
                    })
                    .collect(),
                port_connector_paths: node
                    .component_def
                    .ports
                    .iter()
                    .map(|p| (p.name.clone(), p.msl_path.clone(), p.size_x, p.size_y, p.rotation_deg))
                    .collect(),
                is_conditional: node.is_conditional,
            }),
            ports,
            label: node.instance_name.clone(),
            origin: Some(node.instance_name.clone()),
            // Modelica icons are sized by their `Icon` annotation; a
            // user-driven resize would desync from the source. Plot /
            // dashboard nodes opt into resize via the default `true`.
            resizable: false,
            // Tight halo follows the visible graphics, not the full
            // -100..100 placement frame. Modelica icons commonly only
            // fill ~50 % of their frame (e.g. Tank's body uses
            // -50..50), so a placement-rect halo leaves big empty
            // bands inside the selection box. Apply the same
            // `xform` that mapped icon-local → world to the graphics
            // bbox so rotation / mirror are honoured.
            visual_rect: node
                .component_def
                .icon_graphics
                .as_ref()
                .and_then(|icon| icon.graphics_bbox())
                .map(|e| {
                    let ((vx0, vy0), (vx1, vy1)) = xform.local_aabb(
                        e.p1.x as f32,
                        e.p1.y as f32,
                        e.p2.x as f32,
                        e.p2.y as f32,
                    );
                    CanvasRect::from_min_max(
                        CanvasPos::new(vx0, vy0),
                        CanvasPos::new(vx1, vy1),
                    )
                }),
        });
    }

    for edge in &diagram.edges {
        let Some(src_cid) = id_map.get(&edge.source_node) else {
            continue;
        };
        let Some(tgt_cid) = id_map.get(&edge.target_node) else {
            continue;
        };

        // Look up the source / target port definitions so we can
        // bake connector type + edge-side direction into the edge's
        // data. The visual reads both for colour selection and
        // port-direction stubs without needing world access.
        let src_node = diagram.nodes.iter().find(|n| n.id == edge.source_node);
        let tgt_node = diagram.nodes.iter().find(|n| n.id == edge.target_node);
        // Port lookup falls back to the head segment so qualified
        // sub-port references like `flange.phi` (from
        // `recover_edges_from_source`) still resolve to the
        // outer `flange` PortDef. Without this, every recovered
        // edge with a sub-port lost its colour + stub direction
        // because the find() returned None.
        let find_port = |defs: &[crate::visual_diagram::PortDef], name: &str|
            -> Option<crate::visual_diagram::PortDef>
        {
            if let Some(p) = defs.iter().find(|p| p.name == name) {
                return Some(p.clone());
            }
            let head = name.split('.').next().unwrap_or(name);
            defs.iter().find(|p| p.name == head).cloned()
        };
        let src_port_def =
            src_node.and_then(|n| find_port(&n.component_def.ports, &edge.source_port));
        let tgt_port_def =
            tgt_node.and_then(|n| find_port(&n.component_def.ports, &edge.target_port));
        let connector_type = src_port_def
            .as_ref()
            .map(|p| p.connector_type.clone())
            .unwrap_or_default();
        // Wire color sourced from the connector class's Icon
        // (populated by the projector for both local & MSL types).
        // Falls back to `null` so the edge factory uses the leaf-name
        // palette in `wire_color_for`.
        let icon_color = src_port_def
            .as_ref()
            .and_then(|p| p.color)
            .or_else(|| tgt_port_def.as_ref().and_then(|p| p.color));
        // Stub direction = which edge the port sits on in *screen*
        // space. Apply the owning instance's transform's linear part
        // (no translation — directions don't have a position). One
        // matrix multiply per port replaces the previous four
        // per-feature branches (mirror_x, mirror_y, rotate_x, …).
        let from_dir = match (src_node, src_port_def.as_ref()) {
            (Some(n), Some(p)) => {
                let (dx, dy) = n.icon_transform.apply_dir(p.x, p.y);
                port_edge_dir(dx, dy)
            }
            _ => PortDir::None,
        };
        let to_dir = match (tgt_node, tgt_port_def.as_ref()) {
            (Some(n), Some(p)) => {
                let (dx, dy) = n.icon_transform.apply_dir(p.x, p.y);
                port_edge_dir(dx, dy)
            }
            _ => PortDir::None,
        };

        let eid = scene.alloc_edge_id();

        // Auto-route waypoints when the source has no authored
        // `Line(points={...})` annotation. A* on a 4-unit grid with
        // a bend penalty + obstacle inflation produces clean L / Z /
        // wrap-around routes that the per-frame Z-bend heuristic
        // can't manage. Authored waypoints win — preserves user
        // intent on hand-routed connections.
        let waypoints_world: Vec<CanvasPos> = if !edge.waypoints.is_empty() {
            edge.waypoints
                .iter()
                .map(|&(x, y)| CanvasPos::new(x, -y))
                .collect()
        } else {
            // World endpoints: port world position via owning
            // node's transform.
            let src_world = src_node
                .and_then(|n| src_port_def.as_ref().map(|p| n.icon_transform.apply(p.x, p.y)));
            let tgt_world = tgt_node
                .and_then(|n| tgt_port_def.as_ref().map(|p| n.icon_transform.apply(p.x, p.y)));
            match (src_world, tgt_world) {
                (Some(s), Some(t)) => {
                    let from_out = from_dir.outward();
                    let to_out = to_dir.outward();
                    let obstacles: Vec<crate::ui::wire_router::Obstacle> = scene
                        .nodes()
                        .filter(|(id, _)| **id != *src_cid && **id != *tgt_cid)
                        .filter(|(_, n)| n.kind.as_str() == "modelica.icon")
                        .map(|(_, n)| {
                            let r = n.visual_rect.unwrap_or(n.rect);
                            crate::ui::wire_router::Obstacle {
                                min_x: r.min.x,
                                min_y: r.min.y,
                                max_x: r.max.x,
                                max_y: r.max.y,
                            }
                        })
                        .collect();
                    // grid 4 / bend 80 / clearance 2: bend penalty
                    // is 20× the step cost so A* very strongly prefers
                    // 1- or 2-bend routes over wrappy multi-bend ones.
                    // Earlier value (16) was tied with relatively short
                    // detours, so the green Tank.mass_out wire took a
                    // 4-bend wrap around the engine when a 2-bend
                    // route over the top was available.
                    let pts = crate::ui::wire_router::route(
                        s,
                        from_out,
                        t,
                        to_out,
                        &obstacles,
                        4.0,
                        80.0,
                        2.0,
                    );
                    // Strip endpoints — `waypoints_world` carries
                    // *interior* bends only; the renderer prepends /
                    // appends the actual port positions.
                    if pts.len() >= 2 {
                        pts[1..pts.len() - 1]
                            .iter()
                            .map(|&(x, y)| CanvasPos::new(x, y))
                            .collect()
                    } else {
                        Vec::new()
                    }
                }
                _ => Vec::new(),
            }
        };
        scene.insert_edge(CanvasEdge {
            id: eid,
            from: PortRef {
                node: *src_cid,
                port: CanvasPortId::new(edge.source_port.clone()),
            },
            to: PortRef {
                node: *tgt_cid,
                port: CanvasPortId::new(edge.target_port.clone()),
            },
            kind: "modelica.connection".into(),
            data: std::sync::Arc::new(ConnectionEdgeData {
                connector_type: connector_type.clone(),
                from_dir,
                to_dir,
                waypoints_world,
                icon_color: icon_color
                    .map(|[r, g, b]| egui::Color32::from_rgb(r, g, b)),
                source_path: src_node
                    .map(|n| format!("{}.{}", n.instance_name, edge.source_port))
                    .unwrap_or_default(),
                target_path: tgt_node
                    .map(|n| format!("{}.{}", n.instance_name, edge.target_port))
                    .unwrap_or_default(),
                kind: src_port_def
                    .as_ref()
                    .map(|p| p.kind)
                    .unwrap_or(crate::visual_diagram::PortKind::Acausal),
                flow_vars: src_port_def
                    .as_ref()
                    .map(|p| p.flow_vars.clone())
                    .unwrap_or_default(),
            }),
            origin: None,
        });
    }

    (scene, id_map)
}

// ─── Panel state + Bevy resource ───────────────────────────────────

/// Per-document canvas state. Each open model tab owns one of
/// these, keyed by [`DocumentId`] on [`CanvasDiagramState`]. Holds
/// the transform + selection + in-flight projection task for that
/// specific document so switching tabs doesn't leak viewport,
/// selection, or a stale projection into a neighbour.
/// Shared handle to the target class's `Diagram(graphics={...})`
/// annotation — painted as canvas background by
/// [`DiagramDecorationLayer`]. Projector updates it each time the
/// drilled-in class changes.
pub type BackgroundDiagramHandle = std::sync::Arc<
    std::sync::RwLock<
        Option<(
            crate::annotations::CoordinateSystem,
            Vec<crate::annotations::GraphicItem>,
        )>,
    >,
>;

#[allow(dead_code)]
#[cfg(any())]
fn render_canvas_plots_deprecated(
    ui: &mut bevy_egui::egui::Ui,
    world: &mut World,
    active_doc: Option<lunco_doc::DocumentId>,
    canvas_screen_rect: bevy_egui::egui::Rect,
) {
    use bevy_egui::egui;
    use egui_plot::{Line, Plot, PlotPoints};
    let Some(active_doc) = active_doc else { return };

    // Snapshot plot list + viewport so we don't hold the docstate
    // borrow across egui_plot calls.
    let (plots, viewport) = {
        let state = world.resource::<CanvasDiagramState>();
        let docstate = state.get(Some(active_doc));
        if docstate.canvas_plots.is_empty() {
            return;
        }
        (
            docstate.canvas_plots.clone(),
            docstate.canvas.viewport.clone(),
        )
    };

    // Look up the active simulator entity once — same lookup
    // NewPlotPanel uses to bind signal refs.
    let model_entity = world
        .query::<(bevy::prelude::Entity, &crate::ModelicaModel)>()
        .iter(world)
        .next()
        .map(|(e, _)| e)
        .unwrap_or(bevy::prelude::Entity::PLACEHOLDER);

    let canvas_rect = lunco_canvas::Rect::from_min_max(
        lunco_canvas::Pos::new(canvas_screen_rect.min.x, canvas_screen_rect.min.y),
        lunco_canvas::Pos::new(canvas_screen_rect.max.x, canvas_screen_rect.max.y),
    );

    // Pull SignalRegistry once — it's a Resource we read for every
    // plot below, no mutation.
    let registry_present =
        world.get_resource::<lunco_viz::SignalRegistry>().is_some();
    if !registry_present {
        return;
    }

    for (idx, plot) in plots.iter().enumerate() {
        let screen_rect =
            viewport.world_rect_to_screen(
                lunco_canvas::Rect::from_min_max(plot.world_min, plot.world_max),
                canvas_rect,
            );
        let egui_rect = egui::Rect::from_min_max(
            egui::pos2(screen_rect.min.x, screen_rect.min.y),
            egui::pos2(screen_rect.max.x, screen_rect.max.y),
        );
        // Skip plots fully outside the visible canvas area —
        // pan/zoom can move them off-screen and rendering an
        // off-canvas widget wastes layout time.
        if !canvas_screen_rect.intersects(egui_rect) {
            continue;
        }

        // Build the line points from SignalRegistry. Re-acquire
        // the resource borrow per-plot so future per-plot
        // multi-signal lookups stay simple.
        let signal_ref =
            lunco_viz::SignalRef::new(model_entity, plot.signal_path.clone());
        let points: Vec<[f64; 2]> = world
            .resource::<lunco_viz::SignalRegistry>()
            .scalar_history(&signal_ref)
            .map(|h| h.samples.iter().map(|s| [s.time, s.value]).collect())
            .unwrap_or_default();

        // Foreground layer so the plot draws on top of nodes/wires.
        let fg_layer = egui::LayerId::new(
            egui::Order::Foreground,
            ui.id().with(("canvas_plot", active_doc.raw(), idx)),
        );
        let painter = ui.ctx().layer_painter(fg_layer);
        // Card background so the plot stays readable over busy
        // diagrams. Theme-driven colours come from the canvas
        // overlay theme already used by the NavBar overlay.
        let theme = lunco_canvas::theme::current(ui.ctx());
        painter.rect_filled(egui_rect, 6.0, theme.overlay_fill);
        painter.rect_stroke(
            egui_rect,
            6.0,
            egui::Stroke::new(1.0, theme.overlay_stroke),
            egui::StrokeKind::Outside,
        );

        // Plot body — small egui_plot inside the rect. Title bar
        // shows the bound signal name.
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui_rect.shrink(4.0))
                .layout(egui::Layout::top_down(egui::Align::Min))
                .layer_id(fg_layer),
        );
        child.label(
            egui::RichText::new(&plot.signal_path)
                .small()
                .color(theme.overlay_text),
        );
        let plot_id = (
            "lunco_canvas_plot",
            active_doc.raw(),
            idx as u64,
        );
        Plot::new(plot_id)
            .show_axes([false, false])
            .show_grid(false)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .show(&mut child, |plot_ui| {
                if !points.is_empty() {
                    plot_ui.line(Line::new("", PlotPoints::from(points)));
                }
            });
    }
}

pub struct CanvasDocState {
    pub canvas: Canvas,
    pub last_seen_gen: u64,
    /// Generation that the *canvas scene* already reflects, ahead of
    /// or equal to the AST projection. Bumped by [`apply_ops`] when a
    /// canvas-originated edit has already been applied locally
    /// (drag → SetPlacement leaves the scene moved; menu Add → a
    /// synthesised node is inserted into the scene). The project gate
    /// then skips reprojection while `canvas_acked_gen >= gen`,
    /// which is what keeps Add and Move feeling instant: no waiting
    /// for the off-thread parse to complete before the visual
    /// settles. The next *foreign* edit (typed source change) bumps
    /// `gen` past `canvas_acked_gen` and the regular projection path
    /// re-engages.
    pub canvas_acked_gen: u64,
    /// Hash of the *projection-relevant* slice of source for the
    /// scene currently on screen — collapses whitespace, drops
    /// comments. Cheap-skip: when a doc generation bumps but this
    /// hash is unchanged (a comment edit, a parameter-default tweak,
    /// added blank lines), we mark the gen as seen without spawning
    /// a projection task. Catches the bulk of typing latency.
    ///
    /// TODO(partial-reproject): replace this binary skip with an
    /// AST-diff path. Compare prev vs new `ClassDef.components` /
    /// `equations` / annotations, emit a sequence of
    /// `DiagramOp { AddNode | RemoveNode | MoveNode | AddEdge |
    /// RemoveEdge | RelabelNode }`, and apply each to `canvas.scene`
    /// in place. Falls back to full reproject on extends/within/
    /// multi-class changes. Needs (1) Scene mutation API surface
    /// (move/relabel/add-without-rebuild), (2) `diff_class(old,
    /// new) -> Vec<DiagramOp>` helper, (3) origin-name as stable
    /// node identity (already true). 30 % of edits hit the partial
    /// path — see <follow-up issue> when ready.
    pub last_seen_source_hash: u64,
    /// Set by the [`crate::ui::commands::FitCanvas`] observer; the
    /// canvas render system consumes it next frame and runs Fit
    /// against the *actual* widget rect (rather than the hardcoded
    /// 800×600 the observer would have to use). Cleared after the
    /// fit lands.
    pub pending_fit: bool,
    /// Snapshot of the drill-in target that produced the *currently
    /// rendered* scene. The render trigger compares this against the
    /// live `DrilledInClassNames[doc_id]`; a difference re-projects.
    /// Without this, clicking a class in the Twin Browser updated the
    /// drill-in resource but the canvas kept showing the previous
    /// target's cached scene — the visible "click did nothing" bug.
    pub last_seen_target: Option<String>,
    pub context_menu: Option<PendingContextMenu>,
    pub projection_task: Option<ProjectionTask>,
    /// Background decoration — the target class's own
    /// `Diagram(graphics={...})` annotation. Painted by the
    /// decoration layer registered on `canvas`. Shared via `Arc` so
    /// the projection code can update the layer's data without
    /// reaching into `canvas.layers`.
    pub background_diagram: BackgroundDiagramHandle,
    /// Per-doc pulse-glow registry. The `drive_pending_api_focus`
    /// system writes new entries when an API-driven AddComponent's
    /// node lands in the projected scene; the `PulseGlowLayer` reads
    /// this every draw and paints a Figma-style outer-glow ring with
    /// alpha decaying over `PULSE_DURATION`. Shared by `Arc` so both
    /// sides see the same vec without walking the layer list.
    pub pulse_handle: PulseHandle,
    /// Per-doc edge-pulse registry — same shape as `pulse_handle`
    /// but for newly-added connections. Drives the wire-flash
    /// rendered by `EdgePulseLayer`.
    pub edge_pulse_handle: EdgePulseHandle,
}

impl Default for CanvasDocState {
    fn default() -> Self {
        let mut canvas = Canvas::new(build_registry());
        canvas.layers.retain(|layer| layer.name() != "selection");
        canvas.overlays.push(Box::new(NavBarOverlay::default()));
        // Diagram decoration layer sits right after the grid so it
        // paints behind nodes and edges. The decoration data is
        // shared via `Arc<RwLock>` with `CanvasDocState` so the
        // projector can swap in a new class's graphics without
        // walking the layer list.
        let background_diagram: BackgroundDiagramHandle =
            std::sync::Arc::new(std::sync::RwLock::new(None));
        let decoration_idx = canvas
            .layers
            .iter()
            .position(|l| l.name() != "grid")
            .unwrap_or(canvas.layers.len());
        canvas.layers.insert(
            decoration_idx,
            Box::new(DiagramDecorationLayer {
                data: background_diagram.clone(),
            }),
        );
        // Pulse-glow layer goes at the END of the layer list so the
        // ring paints ON TOP of nodes/edges/selection — matches
        // Figma's outer-glow which is visible regardless of underlying
        // chrome. See `docs/architecture/20-domain-modelica.md` § 9c.4.
        let pulse_handle: PulseHandle =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        canvas.layers.push(Box::new(PulseGlowLayer {
            data: pulse_handle.clone(),
        }));
        let edge_pulse_handle: EdgePulseHandle =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        canvas.layers.push(Box::new(EdgePulseLayer {
            data: edge_pulse_handle.clone(),
        }));
        Self {
            canvas,
            last_seen_gen: 0,
            canvas_acked_gen: 0,
            last_seen_source_hash: 0,
            pending_fit: false,
            last_seen_target: None,
            context_menu: None,
            projection_task: None,
            background_diagram,
            pulse_handle,
            edge_pulse_handle,
        }
    }
}

/// Hash the *projection-relevant* slice of source — collapses runs
/// of whitespace into single spaces and drops `//` line comments
/// and `/* … */` block comments. String literals are preserved
/// (they include filenames in `Bitmap(fileName=...)` annotations,
/// which DO affect rendering).
///
/// Used by the cheap "edit-class skip": when the document
/// generation bumps but this hash hasn't moved, the edit was a
/// comment / blank-line / parameter-default tweak that doesn't
/// change the projected scene topology — skip the projection task
/// entirely. Catches the bulk of the typing-latency regressions on
/// large MSL files.
///
/// Note: false negatives (edits that DO change projection but
/// produce the same hash) are impossible — the hash domain
/// includes every glyph in components / equations / annotations.
/// False positives (edits that DON'T change projection but bump
/// the hash) are fine: we just over-project, same as before.
fn projection_relevant_source_hash(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    let mut last_was_ws = true;
    while let Some(c) = chars.next() {
        if in_string {
            c.hash(&mut h);
            if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    chars.next();
                    while let Some(&n) = chars.peek() {
                        if n == '\n' { break; }
                        chars.next();
                    }
                    continue;
                }
                Some('*') => {
                    chars.next();
                    while let Some(c2) = chars.next() {
                        if c2 == '*' && chars.peek() == Some(&'/') {
                            chars.next();
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        if c == '"' {
            in_string = true;
            c.hash(&mut h);
            last_was_ws = false;
            continue;
        }
        if c.is_whitespace() {
            if !last_was_ws {
                ' '.hash(&mut h);
                last_was_ws = true;
            }
            continue;
        }
        c.hash(&mut h);
        last_was_ws = false;
    }
    h.finish()
}

/// Paints the target class's `Diagram(graphics={...})` annotation as
/// canvas background — the red labelled rectangles, text callouts,
/// and accent lines MSL example diagrams carry for reader orientation
/// (the PID example's "reference speed generation" / "PI controller"
/// / "plant" regions are the canonical case). Holds an
/// `Arc<RwLock<…>>` handle so the projector can push a new class's
/// graphics in without reaching into the canvas's layer list.
struct DiagramDecorationLayer {
    data: BackgroundDiagramHandle,
}

impl lunco_canvas::Layer for DiagramDecorationLayer {
    fn name(&self) -> &'static str {
        "modelica.diagram_decoration"
    }
    fn draw(
        &mut self,
        ctx: &mut lunco_canvas::visual::DrawCtx,
        _scene: &lunco_canvas::Scene,
        _selection: &lunco_canvas::Selection,
    ) {
        let Ok(guard) = self.data.read() else { return };
        let Some((coord_system, graphics)) = guard.as_ref() else {
            return;
        };
        // Map the coordinate system's extent (Modelica +Y up) to the
        // canvas world rect (+Y down) by flipping Y. Our node
        // placements already live in this flipped space, so the
        // decoration lines up with the nodes natively.
        let ext = coord_system.extent;
        let world_min_x = (ext.p1.x.min(ext.p2.x)) as f32;
        let world_max_x = (ext.p1.x.max(ext.p2.x)) as f32;
        let world_min_y = -(ext.p1.y.max(ext.p2.y) as f32);
        let world_max_y = -(ext.p1.y.min(ext.p2.y) as f32);
        let world_rect = lunco_canvas::Rect::from_min_max(
            lunco_canvas::Pos::new(world_min_x, world_min_y),
            lunco_canvas::Pos::new(world_max_x, world_max_y),
        );
        let screen_rect_canvas =
            ctx.viewport.world_rect_to_screen(world_rect, ctx.screen_rect);
        let screen_rect = bevy_egui::egui::Rect::from_min_max(
            bevy_egui::egui::pos2(screen_rect_canvas.min.x, screen_rect_canvas.min.y),
            bevy_egui::egui::pos2(screen_rect_canvas.max.x, screen_rect_canvas.max.y),
        );
        // Filter out items that have a corresponding scene Node
        // (Text → editable label, LunCoPlotNode → live plot tile).
        // Painting them here as well would double-render: the
        // scene Node already paints itself via its `NodeVisual`
        // and the decoration would just sit on top with stale
        // text. Other graphics (Rectangle / Line / Polygon /
        // Ellipse / Bitmap) stay as background decoration.
        use crate::annotations::GraphicItem;
        let decoration: Vec<GraphicItem> = graphics
            .iter()
            .filter(|g| {
                !matches!(
                    g,
                    GraphicItem::Text(_) | GraphicItem::LunCoPlotNode(_)
                )
            })
            .cloned()
            .collect();
        crate::icon_paint::paint_graphics(
            ctx.ui.painter(),
            screen_rect,
            *coord_system,
            &decoration,
        );
    }
}

/// Per-panel state carried across frames. Stored as a Bevy resource
/// so the panel's `render` can pull it out via `world.resource_mut`.
///
/// State is sharded per-document — each open model tab has its own
/// [`CanvasDocState`] entry so viewport/selection/projection/context
/// menu never bleed between tabs. `fallback` is used only when no
/// document is bound (startup, every tab closed).
#[derive(Resource, Default)]
pub struct CanvasDiagramState {
    per_doc: std::collections::HashMap<lunco_doc::DocumentId, CanvasDocState>,
    fallback: CanvasDocState,
}

impl CanvasDiagramState {
    /// Read-only view of the state for a given doc. Falls back to an
    /// empty canvas when `doc` is `None` or no entry exists yet — used
    /// during the one-frame window between panel mount and first
    /// projection.
    pub fn get(&self, doc: Option<lunco_doc::DocumentId>) -> &CanvasDocState {
        doc.and_then(|d| self.per_doc.get(&d)).unwrap_or(&self.fallback)
    }

    /// Mutable view, creating the entry on first access. `None` routes
    /// writes to the shared fallback so "no doc bound" doesn't crash.
    pub fn get_mut(
        &mut self,
        doc: Option<lunco_doc::DocumentId>,
    ) -> &mut CanvasDocState {
        match doc {
            Some(d) => self.per_doc.entry(d).or_default(),
            None => &mut self.fallback,
        }
    }

    /// Drop a doc's entry when its document is removed from the
    /// registry (tab closed, file unloaded). Called from
    /// [`cleanup_removed_documents`].
    pub fn drop_doc(&mut self, doc: lunco_doc::DocumentId) {
        self.per_doc.remove(&doc);
    }

    /// Iterate the document ids that currently have canvas state.
    /// Used by the vello-canvas plugin to allocate / reclaim
    /// per-tab render targets.
    pub fn iter_doc_ids(&self) -> impl Iterator<Item = lunco_doc::DocumentId> + '_ {
        self.per_doc.keys().copied()
    }

    /// Read-only state for an explicit doc id, or `None` when no
    /// canvas state exists yet. Distinguishes "doc absent" from
    /// "doc present but empty", which the fallback-returning `get`
    /// can't.
    pub fn get_for_doc(
        &self,
        doc: lunco_doc::DocumentId,
    ) -> Option<&CanvasDocState> {
        self.per_doc.get(&doc)
    }

    /// Has this doc ever been projected? `false` until
    /// `get_mut(Some(doc))` inserts — the trigger the render loop
    /// uses to force an initial projection.
    pub fn has_entry(&self, doc: lunco_doc::DocumentId) -> bool {
        self.per_doc.contains_key(&doc)
    }
}

// ─── Animation: pending API-focus queue ────────────────────────────────
//
// Implements the `OpOrigin::Api` half of `docs/architecture/20-domain-modelica.md`
// § 9c.5 (batch focus debounce). When an API caller adds a component,
// the API command observer writes a `PendingApiFocus` entry; this
// canvas-side system polls it each frame and, once the named node has
// landed in the projected scene, calls `viewport.set_target` to ease
// the camera onto it. The viewport's built-in tween (`set_target` +
// `tick` in `lunco-canvas/viewport.rs`) handles the actual smoothing
// — there is no separate animation system here.
//
// Why a queue rather than a direct-call observer: `AddComponent`
// applies synchronously, but the canvas reprojects asynchronously
// (off-thread parse → `projection_task`), so the new node isn't in
// `scene` for one or more frames. The queue waits patiently and
// applies the focus the moment the node appears.
//
// Batch debounce: when the queue contains multiple entries within a
// `BATCH_WINDOW`, the focus collapses to a single FitVisible over the
// accumulated set instead of ping-ponging between centroids — so a
// scripted N-component build animates into a single framed shot at
// the end. See § 9c.5 for the full rationale.
//
// TODO(modelica.canvas.add.focus_behavior): make None / Center /
// FitVisible settings-driven. Today the policy is hardcoded:
// single-add → Center, batch → FitVisible.
//
// TODO(modelica.canvas.add.batch_debounce_ms): expose `BATCH_WINDOW`
// as a setting.
//
// Pulse glow (§ 9c.4): the focus driver below also pushes matched
// (NodeId, started_at) pairs into the per-doc `PulseHandle`. The
// `PulseGlowLayer` (registered last in the canvas's layer list, so it
// paints on top) walks those entries each frame and draws a soft
// outer ring with alpha decaying linearly over `PULSE_DURATION`.
//
// TODO(modelica.canvas.animation.pulse_ms): expose `PULSE_DURATION`
// as a setting (0 = disable). Today it's hardcoded to 1.0 s.

/// One pending camera focus, queued by an API caller, drained by the
/// canvas's per-frame system once the projection settles.
#[derive(Debug, Clone)]
pub struct PendingApiFocus {
    /// Document the new component lives in.
    pub doc: lunco_doc::DocumentId,
    /// Component instance name (matches `Node.origin` after projection).
    pub name: String,
    /// When the API caller queued this. Used both for batch debounce
    /// and timeout GC.
    pub queued_at: web_time::Instant,
    /// Per-call pulse glow duration (ms). 0 disables the glow for
    /// this entry. Defaults to `DEFAULT_PULSE_MS` when the API
    /// caller didn't supply `animation_ms`.
    pub animation_ms: u32,
}

/// FIFO queue of pending API-driven focuses. `ApiEdits::on_add_modelica_component`
/// pushes; the canvas's `drive_pending_api_focus` system drains.
///
/// Kept as a `Vec` not a `HashMap` so order is preserved — batch debounce
/// needs to know whether the *latest* push is recent enough to coalesce.
#[derive(Resource, Default)]
pub struct PendingApiFocusQueue(pub Vec<PendingApiFocus>);

impl PendingApiFocusQueue {
    pub fn push(&mut self, focus: PendingApiFocus) {
        self.0.push(focus);
    }
}

/// Window for batch-collapse: if a new entry arrives within this of
/// the previous one, the system holds back from focusing on the older
/// entries individually and instead waits for the burst to end.
const BATCH_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);

/// Hard timeout — drop a queued focus if no node with the given origin
/// has appeared in the scene by then. Stops the queue from leaking on
/// failed AddComponent ops or rename races.
const FOCUS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Default pulse duration when the API caller doesn't override it.
/// Per-call override lives on `AddModelicaComponent.animation_ms` /
/// `ConnectComponents.animation_ms`; 0 disables the highlight
/// entirely. Quartic slow-tail (`alpha = 1 - t^4`) decay regardless
/// of total length.
pub const DEFAULT_PULSE_MS: u32 = 2000;
pub const DEFAULT_EDGE_FLASH_MS: u32 = 1500;

/// Stagger between consecutive node-pulse start times within a batch.
/// Adds a "slight delay between elements" feel (per user feedback)
/// without actually delaying the source mutation — the components
/// land in the scene at once; the *pulse* is what reveals them in
/// sequence. Empty for batch=1.
const PULSE_STAGGER_MS: u64 = 250;

/// Connection-add queue (mirror of `PendingApiFocusQueue` but for
/// `ConnectComponents`). The driver matches each entry against the
/// scene's edge list (by from/to component+port) and pushes a brief
/// flash entry into the doc's `edge_pulse_handle`.
#[derive(Resource, Default)]
pub struct PendingApiConnectionQueue(pub Vec<PendingApiConnection>);

#[derive(Debug, Clone)]
pub struct PendingApiConnection {
    pub doc: lunco_doc::DocumentId,
    pub from_component: String,
    pub from_port: String,
    pub to_component: String,
    pub to_port: String,
    pub queued_at: web_time::Instant,
    /// Per-call edge-flash duration (ms). 0 = no flash. Defaults to
    /// `DEFAULT_EDGE_FLASH_MS` when not supplied.
    pub animation_ms: u32,
}

impl PendingApiConnectionQueue {
    pub fn push(&mut self, entry: PendingApiConnection) {
        self.0.push(entry);
    }
}

/// Outer-glow render layer for newly-added edges. Re-uses the
/// `PulseGlowLayer`'s decay curve and theme colour, but draws an
/// additional thicker stroke ON TOP of the edge so the wire visibly
/// flashes — see `docs/architecture/20-domain-modelica.md` § 9c.4.
struct EdgePulseLayer {
    data: EdgePulseHandle,
}

impl lunco_canvas::Layer for EdgePulseLayer {
    fn name(&self) -> &'static str {
        "modelica.edge_pulse"
    }

    fn draw(
        &mut self,
        ctx: &mut lunco_canvas::visual::DrawCtx,
        scene: &lunco_canvas::Scene,
        _selection: &lunco_canvas::Selection,
    ) {
        let live: Vec<(lunco_canvas::EdgeId, f32)> = {
            let Ok(mut guard) = self.data.write() else {
                return;
            };
            let now = web_time::Instant::now();
            guard.retain(|e| match now.checked_duration_since(e.started) {
                Some(d) => d.as_millis() < e.duration_ms as u128,
                None => true,
            });
            guard
                .iter()
                .map(|e| {
                    let alpha = match now.checked_duration_since(e.started) {
                        None => 0.0,
                        Some(elapsed) => {
                            let age_ms = elapsed.as_secs_f32() * 1000.0;
                            let total_ms = (e.duration_ms as f32).max(1.0);
                            let t = (age_ms / total_ms).clamp(0.0, 1.0);
                            1.0 - t.powi(4)
                        }
                    };
                    (e.id, alpha)
                })
                .filter(|(_, a)| *a > 0.001)
                .collect()
        };
        if live.is_empty() {
            return;
        }
        let painter = ctx.ui.painter();
        let _theme = lunco_canvas::theme::current(ctx.ui.ctx());
        // Warm yellow-orange — distinct from the wire's blue so the
        // flash reads as a *highlight*, not a thicker wire. Picked
        // for high contrast against both light and dark themes.
        // (`theme.selection_outline` matched too closely to the wire
        // colour and the user reported the flash didn't register.)
        let base = bevy_egui::egui::Color32::from_rgb(255, 196, 60);
        for (edge_id, alpha) in live {
            let Some(edge) = scene.edge(edge_id) else {
                continue;
            };
            // Look up the two endpoints' world positions via their
            // owning nodes' rects + the port's local offset. If
            // either endpoint is missing (race during projection),
            // skip silently.
            let Some(from_node) = scene.node(edge.from.node) else {
                continue;
            };
            let Some(to_node) = scene.node(edge.to.node) else {
                continue;
            };
            let from_world = port_world_pos(from_node, &edge.from.port);
            let to_world = port_world_pos(to_node, &edge.to.port);
            let Some(from_world) = from_world else { continue };
            let Some(to_world) = to_world else { continue };
            let from_screen = ctx
                .viewport
                .world_to_screen(from_world, ctx.screen_rect);
            let to_screen = ctx.viewport.world_to_screen(to_world, ctx.screen_rect);

            // Two stacked strokes — fat outer halo + tighter bright
            // inner line. Distinct yellow-orange so the highlight
            // reads against the wire's blue body.
            let halo_a = (alpha * 0.7 * 255.0) as u8;
            let line_a = (alpha * 0.95 * 255.0) as u8;
            painter.line_segment(
                [
                    bevy_egui::egui::pos2(from_screen.x, from_screen.y),
                    bevy_egui::egui::pos2(to_screen.x, to_screen.y),
                ],
                bevy_egui::egui::Stroke::new(
                    18.0,
                    bevy_egui::egui::Color32::from_rgba_unmultiplied(
                        base.r(),
                        base.g(),
                        base.b(),
                        halo_a,
                    ),
                ),
            );
            painter.line_segment(
                [
                    bevy_egui::egui::pos2(from_screen.x, from_screen.y),
                    bevy_egui::egui::pos2(to_screen.x, to_screen.y),
                ],
                bevy_egui::egui::Stroke::new(
                    5.0,
                    bevy_egui::egui::Color32::from_rgba_unmultiplied(
                        base.r(),
                        base.g(),
                        base.b(),
                        line_a,
                    ),
                ),
            );
        }
    }
}

/// Resolve a port's world-space position via its owning node's rect
/// + the port's `local_offset`. Canvas convention (see
/// `lunco-canvas::layer::EdgesLayer::draw`): `local_offset` is
/// relative to `rect.min` (top-left), NOT the centre. Falls back to
/// the rect centre when the port id isn't on the node — same fallback
/// the edges layer uses for connector-only nodes.
fn port_world_pos(
    node: &lunco_canvas::Node,
    port_id: &lunco_canvas::PortId,
) -> Option<lunco_canvas::Pos> {
    let port = node.ports.iter().find(|p| &p.id == port_id)?;
    Some(lunco_canvas::Pos::new(
        node.rect.min.x + port.local_offset.x,
        node.rect.min.y + port.local_offset.y,
    ))
}

/// Per-frame driver for connection adds: like
/// `drive_pending_api_focus`, but matches the queue against scene
/// edges and pushes flashes into the edge-pulse handle. No camera
/// move — connections appear in the existing camera frame; their
/// flash is the signal.
pub fn drive_pending_api_connections(
    mut queue: ResMut<PendingApiConnectionQueue>,
    mut state: ResMut<CanvasDiagramState>,
) {
    if queue.0.is_empty() {
        return;
    }
    let now = web_time::Instant::now();
    let mut still_pending: Vec<PendingApiConnection> = Vec::new();
    for entry in queue.0.drain(..) {
        if now.duration_since(entry.queued_at) > FOCUS_TIMEOUT {
            continue;
        }
        let docstate = state.get(Some(entry.doc));
        // Match by node `origin` (component name) + port id (port
        // name). The canvas projection puts the port's name in
        // `Port.id`'s string form via the SmolStr; matching by id
        // works because the projector keys ports by simple name.
        let hit = docstate.canvas.scene.edges().find(|(_, e)| {
            let from_node = docstate.canvas.scene.node(e.from.node);
            let to_node = docstate.canvas.scene.node(e.to.node);
            let from_match = from_node
                .map(|n| {
                    n.origin.as_deref() == Some(entry.from_component.as_str())
                        && n.ports
                            .iter()
                            .any(|p| p.id == e.from.port && p.id.as_str() == entry.from_port.as_str())
                })
                .unwrap_or(false);
            let to_match = to_node
                .map(|n| {
                    n.origin.as_deref() == Some(entry.to_component.as_str())
                        && n.ports
                            .iter()
                            .any(|p| p.id == e.to.port && p.id.as_str() == entry.to_port.as_str())
                })
                .unwrap_or(false);
            from_match && to_match
        });
        match hit {
            Some((edge_id, _)) => {
                let edge_id = *edge_id;
                let anim_ms = entry.animation_ms;
                let docstate_mut = state.get_mut(Some(entry.doc));
                if anim_ms > 0 {
                    if let Ok(mut guard) = docstate_mut.edge_pulse_handle.write() {
                        guard.push(PulseEntry {
                            id: edge_id,
                            started: web_time::Instant::now(),
                            duration_ms: anim_ms,
                        });
                    }
                }
            }
            None => still_pending.push(entry),
        }
    }
    queue.0 = still_pending;
}

// ─── Cinematic camera ──────────────────────────────────────────────────
//
// Replaces `viewport.set_target`'s constant exponential smoothing with
// a keyframe-driven curve. Lets us do shot types — pure dolly, focus
// pull (zoom-out + hold + zoom-in), establishing shot — instead of
// always linearly easing toward the target. Frame-rate independent;
// driven by elapsed wall-clock.
//
// Why a keyframe model: a single `Tween { from, to, duration, ease }`
// can't express the "pull back, hold, push in" shape that makes
// distant targets feel intentional rather than swoopy. Keyframes are
// the standard movie-camera abstraction: anchor a curve at each
// time offset, blend in between.
//
// While a cinematic is active, the viewport's built-in tween must not
// also drift the values, so each frame we snap-set both current AND
// target to the eased keyframe value (`viewport.snap_to`).

#[derive(Clone, Copy, Debug)]
enum EaseKind {
    /// Constant value — produces a "hold" segment between two
    /// identical keyframes.
    Hold,
    /// Linear blend.
    Linear,
    /// Soft-start, hard-end. Good for arrivals.
    EaseOutCubic,
    /// Symmetric soft-start and soft-end. Default for smooth dollies.
    EaseInOutCubic,
}

impl EaseKind {
    fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            EaseKind::Hold => 0.0,
            EaseKind::Linear => t,
            EaseKind::EaseOutCubic => 1.0 - (1.0 - t).powi(3),
            EaseKind::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    1.0 - ((-2.0 * t + 2.0).powi(3)) / 2.0
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Keyframe {
    /// Offset from move start, milliseconds.
    at_ms: u32,
    center: lunco_canvas::Pos,
    zoom: f32,
    /// How to ease *into* this keyframe from the previous one.
    ease_in: EaseKind,
}

struct CameraMove {
    started_at: web_time::Instant,
    keyframes: Vec<Keyframe>,
    /// What we last `snap_to`'d the viewport with. On the next tick,
    /// if `viewport.center / .zoom` no longer matches, the user (or
    /// the on-canvas zoom widget at the bottom-right) moved the
    /// camera — the cinematic yields and stops fighting them.
    last_applied: Option<(lunco_canvas::Pos, f32)>,
}

impl CameraMove {
    fn total_ms(&self) -> u32 {
        self.keyframes.last().map(|k| k.at_ms).unwrap_or(0)
    }
    /// Eased (center, zoom) at the given elapsed time. `None` once
    /// past the last keyframe — caller drops the move then.
    fn sample(&self, elapsed_ms: u32) -> Option<(lunco_canvas::Pos, f32)> {
        if self.keyframes.is_empty() {
            return None;
        }
        if elapsed_ms >= self.total_ms() {
            return None;
        }
        // Find the segment [prev, next] containing elapsed_ms.
        let mut prev = &self.keyframes[0];
        for kf in self.keyframes.iter().skip(1) {
            if elapsed_ms < kf.at_ms {
                let span_ms = (kf.at_ms - prev.at_ms).max(1) as f32;
                let local_t = (elapsed_ms - prev.at_ms) as f32 / span_ms;
                let eased = kf.ease_in.apply(local_t);
                return Some((
                    lerp_pos(prev.center, kf.center, eased),
                    lerp_f32(prev.zoom, kf.zoom, eased),
                ));
            }
            prev = kf;
        }
        Some((prev.center, prev.zoom))
    }
}

fn lerp_pos(a: lunco_canvas::Pos, b: lunco_canvas::Pos, t: f32) -> lunco_canvas::Pos {
    lunco_canvas::Pos::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}
fn lerp_f32(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Per-doc active cinematic move. None = no move (viewport is free).
#[derive(Resource, Default)]
pub struct CinematicCamera {
    moves: std::collections::HashMap<lunco_doc::DocumentId, CameraMove>,
}

/// Single-move cinematic duration: pull the camera from wherever it
/// is to the fit-all view in one smooth sweep.
const FIT_MOVE_MS: u32 = 700;

/// Plan a cinematic for an API-driven add batch.
///
/// Per user feedback: simpler than a per-node tour. One smooth
/// pan-and-zoom from the current viewport to a wide framing of *all*
/// nodes (existing + new). The pulse glow on the new nodes is what
/// signals "this just landed"; the wide view is what gives the user
/// "where in the diagram it landed".
///
/// Returns a 2-keyframe move: current → fit-all over `FIT_MOVE_MS`.
fn plan_camera_move(
    current_center: lunco_canvas::Pos,
    current_zoom: f32,
    fit_all_center: lunco_canvas::Pos,
    fit_all_zoom: f32,
) -> Vec<Keyframe> {
    vec![
        Keyframe {
            at_ms: 0,
            center: current_center,
            zoom: current_zoom,
            ease_in: EaseKind::Linear,
        },
        Keyframe {
            at_ms: FIT_MOVE_MS,
            center: fit_all_center,
            zoom: fit_all_zoom,
            ease_in: EaseKind::EaseInOutCubic,
        },
    ]
}

/// Per-frame system: advance every active cinematic move, snap the
/// owning canvas's viewport to the eased value. When a move's last
/// keyframe is reached, drop it and let the user have free camera.
pub fn tick_cinematic_camera(
    mut cinematic: ResMut<CinematicCamera>,
    mut state: ResMut<CanvasDiagramState>,
) {
    if cinematic.moves.is_empty() {
        return;
    }
    let now = web_time::Instant::now();
    let mut finished: Vec<lunco_doc::DocumentId> = Vec::new();
    for (doc, mv) in cinematic.moves.iter_mut() {
        let docstate = state.get_mut(Some(*doc));
        // User-input yield: if the live viewport doesn't match what
        // we last applied, something else moved it (zoom widget,
        // mouse pan, F-to-fit, scroll). Cancel the cinematic and let
        // the user have the camera.
        if let Some((last_c, last_z)) = mv.last_applied {
            let live_c = docstate.canvas.viewport.center;
            let live_z = docstate.canvas.viewport.zoom;
            let dc = (live_c.x - last_c.x).abs() + (live_c.y - last_c.y).abs();
            let dz = (live_z - last_z).abs();
            if dc > 0.5 || dz > 0.005 {
                finished.push(*doc);
                continue;
            }
        }
        let elapsed = now.duration_since(mv.started_at).as_millis() as u32;
        match mv.sample(elapsed) {
            Some((center, zoom)) => {
                docstate.canvas.viewport.snap_to(center, zoom);
                mv.last_applied = Some((center, zoom));
            }
            None => finished.push(*doc),
        }
    }
    for d in finished {
        cinematic.moves.remove(&d);
    }
}

/// One pulse-glow entry: target id, when it started, and how long it
/// should last (per-call duration; the API caller can pass
/// `animation_ms` on the command to override the default). Splitting
/// per-entry instead of using a single global constant lets callers
/// mix instant adds with cinematic ones.
#[derive(Debug, Clone, Copy)]
pub struct PulseEntry<T> {
    pub id: T,
    pub started: web_time::Instant,
    pub duration_ms: u32,
}

/// Per-doc node-pulse registry. Vec rather than HashMap because we
/// expect ≤ a few entries at a time and iteration order doesn't
/// matter — the layer re-walks every frame anyway.
pub type PulseHandle =
    std::sync::Arc<std::sync::RwLock<Vec<PulseEntry<lunco_canvas::NodeId>>>>;

/// Edge-pulse registry: same shape as `PulseHandle` but keyed by edge
/// id. Drives the wire-flash animation when `ConnectComponents` fires
/// from an API caller.
pub type EdgePulseHandle =
    std::sync::Arc<std::sync::RwLock<Vec<PulseEntry<lunco_canvas::EdgeId>>>>;

/// Outer-glow render layer: paints a soft ring around each
/// recently-added node, alpha decaying linearly to 0 over
/// `PULSE_DURATION`. Figma-style — see `docs/architecture/20-domain-modelica.md`
/// § 9c.4 for the design rationale.
struct PulseGlowLayer {
    data: PulseHandle,
}

impl lunco_canvas::Layer for PulseGlowLayer {
    fn name(&self) -> &'static str {
        "modelica.pulse_glow"
    }

    fn draw(
        &mut self,
        ctx: &mut lunco_canvas::visual::DrawCtx,
        scene: &lunco_canvas::Scene,
        _selection: &lunco_canvas::Selection,
    ) {
        // First, walk + decay; collect (node_id, alpha) for entries
        // still alive. Drop the write guard before any heavy painting.
        let live: Vec<(lunco_canvas::NodeId, f32)> = {
            let Ok(mut guard) = self.data.write() else {
                return;
            };
            let now = web_time::Instant::now();
            // Drop entries whose start+duration has elapsed. Entries
            // whose `started` is still in the future stay (they were
            // staggered by the focus driver — see PULSE_STAGGER_MS).
            // Per-entry duration: each call carries its own
            // `duration_ms` so a caller can pass `animation_ms = 500`
            // for a quick add or `animation_ms = 0` to skip the
            // glow.
            guard.retain(|e| match now.checked_duration_since(e.started) {
                Some(d) => d.as_millis() < e.duration_ms as u128,
                None => true,
            });
            guard
                .iter()
                .map(|e| {
                    let alpha = match now.checked_duration_since(e.started) {
                        None => 0.0,
                        Some(elapsed) => {
                            let age_ms = elapsed.as_secs_f32() * 1000.0;
                            let total_ms = (e.duration_ms as f32).max(1.0);
                            let t = (age_ms / total_ms).clamp(0.0, 1.0);
                            1.0 - t.powi(4)
                        }
                    };
                    (e.id, alpha)
                })
                .filter(|(_, a)| *a > 0.001)
                .collect()
        };
        if live.is_empty() {
            return;
        }
        let painter = ctx.ui.painter();
        let theme = lunco_canvas::theme::current(ctx.ui.ctx());
        // Use the theme's selection color as the glow base — ties
        // visually to the rest of the canvas chrome and shifts with
        // the active theme. Multiplied by per-entry alpha and a
        // global pulse intensity (0.65) so the glow stays subtle.
        let base = theme.selection_outline;
        for (node_id, alpha) in live {
            let Some(node) = scene.node(node_id) else {
                continue;
            };
            let world_rect = node.rect;
            let screen = ctx
                .viewport
                .world_rect_to_screen(world_rect, ctx.screen_rect);
            let r = bevy_egui::egui::Rect::from_min_max(
                bevy_egui::egui::pos2(screen.min.x, screen.min.y),
                bevy_egui::egui::pos2(screen.max.x, screen.max.y),
            );
            // Stack 4 expanding outlines with decreasing alpha — the
            // cheapest convincing outer-glow you can do with egui's
            // stroke API. Each layer doubles its outset and halves its
            // opacity, producing a soft falloff.
            for ring in 0..4 {
                let outset = (ring as f32 + 1.0) * 3.0;
                let ring_rect = r.expand(outset);
                let ring_alpha = alpha * 0.65 * (1.0 - ring as f32 * 0.22);
                let a = (ring_alpha * 255.0).clamp(0.0, 255.0) as u8;
                let color = bevy_egui::egui::Color32::from_rgba_unmultiplied(
                    base.r(),
                    base.g(),
                    base.b(),
                    a,
                );
                painter.rect_stroke(
                    ring_rect,
                    bevy_egui::egui::CornerRadius::same(2),
                    bevy_egui::egui::Stroke::new(2.0, color),
                    bevy_egui::egui::StrokeKind::Outside,
                );
            }
        }
    }
}

/// Per-frame driver: drain the focus queue once a *complete* batch has
/// landed in the projected scene, then act ONCE for the whole batch.
/// Designed to avoid the "camera jumps between nodes" feel when N
/// AddComponents arrive across several frames with staggered
/// projection latency.
///
/// Sequence:
///   1. Hold the queue until the latest push is `BATCH_WINDOW` idle.
///   2. Try to match every queued entry. If any is unmatched and not
///      timed out, defer one more frame — keeps the batch atomic.
///   3. Once all matched (or timed out): drain, pulse all, decide the
///      camera move:
///        a. New nodes already inside the viewport → no camera move
///           (Figma/Miro convention — pulse alone signals the change).
///        b. Otherwise → smooth FitVisible over the union of (current
///           visible region ∪ new nodes), so context is preserved.
pub fn drive_pending_api_focus(
    mut queue: ResMut<PendingApiFocusQueue>,
    mut state: ResMut<CanvasDiagramState>,
    mut cinematic: ResMut<CinematicCamera>,
) {
    if queue.0.is_empty() {
        return;
    }
    let now = web_time::Instant::now();

    // (1) Batch-idle gate.
    if let Some(latest) = queue.0.last() {
        if now.duration_since(latest.queued_at) < BATCH_WINDOW {
            return;
        }
    }

    // (2) Try-match pass — non-draining. Anything unmatched and within
    // FOCUS_TIMEOUT forces us to wait one more frame.
    // Match payload now carries per-entry `animation_ms` so the
    // pulse layer entry can use the API caller's override (or 0 to
    // skip the glow entirely).
    let mut matched: std::collections::HashMap<
        lunco_doc::DocumentId,
        Vec<(lunco_canvas::NodeId, lunco_canvas::Pos, lunco_canvas::Rect, u32)>,
    > = std::collections::HashMap::new();
    let mut any_still_unmatched_within_timeout = false;
    for entry in queue.0.iter() {
        let docstate = state.get(Some(entry.doc));
        let hit = docstate
            .canvas
            .scene
            .nodes()
            .find(|(_, n)| n.origin.as_deref() == Some(entry.name.as_str()))
            .map(|(id, node)| (*id, node.rect.center(), node.rect, entry.animation_ms));
        match hit {
            Some(payload) => {
                matched.entry(entry.doc).or_default().push(payload);
            }
            None => {
                if now.duration_since(entry.queued_at) <= FOCUS_TIMEOUT {
                    any_still_unmatched_within_timeout = true;
                }
            }
        }
    }
    if any_still_unmatched_within_timeout {
        return;
    }

    // (3) Whole batch resolved (or timed out). Drain + act.
    queue.0.clear();
    if matched.is_empty() {
        return;
    }

    let now_pulse = web_time::Instant::now();
    let _ = cinematic; // kept for shape; not used now that the camera
                       // move delegates to `pending_fit` (next frame's
                       // canvas render does the math against the real
                       // widget rect — see commentary below).
    for (doc, entries) in matched {
        let docstate = state.get_mut(Some(doc));

        // Always pulse — that's the "what changed" signal. Stagger
        // the start time across entries so a batch reveals
        // one-by-one rather than all flaring at once. Reads as a
        // brief "delay between adds" without delaying the source
        // mutation. Entry order matches the order the API caller
        // queued them.
        if let Ok(mut guard) = docstate.pulse_handle.write() {
            for (i, (node_id, _, _, anim_ms)) in entries.iter().enumerate() {
                if *anim_ms == 0 {
                    continue;
                }
                let stagger = std::time::Duration::from_millis(
                    PULSE_STAGGER_MS * i as u64,
                );
                guard.push(PulseEntry {
                    id: *node_id,
                    started: now_pulse + stagger,
                    duration_ms: *anim_ms,
                });
            }
        }

        // Camera move: defer to the canvas render's `pending_fit`
        // branch. That branch runs INSIDE the panel render where the
        // actual `response.rect` is in scope, so the fit math uses the
        // real widget size — not the 1280×800 approximation we'd have
        // to guess at here. It calls `viewport.set_target`, which
        // animates via the viewport's built-in exponential ease.
        //
        // Why we don't keyframe-cinematic this any more: hardcoding
        // a fake screen rect produced wrong final zoom (the rocket
        // build session showed this — components ended at 37%
        // clipping the labels). The render path's actual-rect fit is
        // the reliable framing.
        //
        // Pulse + edge flash + viewport's smooth ease together carry
        // the visual cadence; the bespoke `plan_camera_move` keyframes
        // are kept in the file for future use (cinematic tours of
        // existing content, etc.) but bypassed for API add-flow.
        docstate.pending_fit = true;
    }
}

/// World-space rect currently visible given the viewport's center,
/// zoom, and the screen extent. Reverse of `world_rect_to_screen`.
fn world_view_rect(
    viewport: &lunco_canvas::Viewport,
    screen: lunco_canvas::Rect,
) -> lunco_canvas::Rect {
    let half_w = (screen.max.x - screen.min.x) * 0.5
        / viewport.zoom.max(f32::EPSILON);
    let half_h = (screen.max.y - screen.min.y) * 0.5
        / viewport.zoom.max(f32::EPSILON);
    let c = viewport.center;
    lunco_canvas::Rect::from_min_max(
        lunco_canvas::Pos::new(c.x - half_w, c.y - half_h),
        lunco_canvas::Pos::new(c.x + half_w, c.y + half_h),
    )
}

/// Strict containment with a small slack — `inner` is considered
/// inside `outer` only if every edge of `inner` is at least `MARGIN`
/// world-units inset from `outer`'s edges. Avoids "technically visible
/// but clipped at the corner" cases where the user would still want a
/// small pan.
fn rect_contains_rect(
    outer: lunco_canvas::Rect,
    inner: lunco_canvas::Rect,
) -> bool {
    const MARGIN: f32 = 20.0;
    inner.min.x >= outer.min.x + MARGIN
        && inner.min.y >= outer.min.y + MARGIN
        && inner.max.x <= outer.max.x - MARGIN
        && inner.max.y <= outer.max.y - MARGIN
}

/// Running projection task + the generation that spawned it, so the
/// poll loop can tell whether we've moved on since and should drop a
/// stale result. The owning doc is implicit: each task lives on that
/// doc's [`CanvasDocState`].
///
/// # Cancellation
///
/// Bevy tasks can't be preempted — "cancel" is cooperative. We
/// give the task a shared `AtomicBool` and a deadline; it polls
/// them at phase boundaries (build → edges recovery → project)
/// and returns an empty `Scene` if either fires. The poll loop
/// drops the handle when the deadline elapses even if the task
/// hasn't noticed yet — the pool runs it to completion but nobody
/// reads the result.
///
/// Two independent "stop" signals:
///
/// - **`cancel`** — flipped to `true` explicitly (user hits
///   cancel, new generation supersedes, tab closed, etc.).
/// - **`deadline`** — wall-clock elapsed > configured max. Reads
///   live via `spawned_at.elapsed() > deadline`.
pub struct ProjectionTask {
    pub gen_at_spawn: u64,
    /// Drill-in target the projection was spawned for. Compared
    /// against `CanvasDocState::last_seen_target` on completion so
    /// the UI knows which target produced the rendered scene.
    pub target_at_spawn: Option<String>,
    pub spawned_at: web_time::Instant,
    pub deadline: std::time::Duration,
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub task: bevy::tasks::Task<Scene>,
    /// Projection-relevant source hash captured at spawn time.
    /// Stashed onto `CanvasDocState::last_seen_source_hash` when the
    /// task completes — used by the next gen-bump check to skip
    /// reprojection on no-op edits (whitespace, comments).
    pub source_hash: u64,
}

/// Snapshot of a right-click: where to anchor the popup + what it
/// was targeted at. Close handling is done via egui's
/// `clicked_elsewhere()` on the popup's Response — no manual timer.
#[derive(Debug, Clone)]
pub struct PendingContextMenu {
    pub screen_pos: egui::Pos2,
    /// World position at click time — used when an "Add component"
    /// entry is selected so the new component lands where the user
    /// right-clicked, not at (0,0).
    pub world_pos: lunco_canvas::Pos,
    pub target: ContextMenuTarget,
}

#[derive(Debug, Clone)]
pub enum ContextMenuTarget {
    Node(lunco_canvas::NodeId),
    Edge(lunco_canvas::EdgeId),
    Empty,
}


// ─── Panel ─────────────────────────────────────────────────────────

pub struct CanvasDiagramPanel;

impl Panel for CanvasDiagramPanel {
    fn id(&self) -> PanelId {
        CANVAS_DIAGRAM_PANEL_ID
    }
    fn title(&self) -> String {
        "🧩 Canvas Diagram".into()
    }
    fn default_slot(&self) -> PanelSlot {
        PanelSlot::Center
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        // TEMP diagnostic: time the whole render and shout when slow.
        // 16ms = 60 fps budget; anything past 30ms is a "felt" stall.
        // Combined with the per-section timers below this tells us
        // exactly which sub-block burns the frame on multi-class docs
        // after a structural op.
        let _frame_t0 = web_time::Instant::now();
        // Ensure the state resource exists before we poke it.
        if world.get_resource::<CanvasDiagramState>().is_none() {
            world.insert_resource(CanvasDiagramState::default());
        }

        // Decide whether to rebuild the scene. Per-doc state means
        // "bound_doc" is implicit in the map key — a fresh entry has
        // `last_seen_gen == 0` so the first render after tab open
        // always re-projects.
        let project_now = {
            // Active doc from the Workspace session (source of truth);
            // `WorkbenchState.open_model` is still read below for
            // display-cache fields, but no longer for identity.
            let Some(doc_id) = world
                .resource::<lunco_workbench::WorkspaceResource>()
                .active_document
            else {
                world
                    .resource_mut::<CanvasDiagramState>()
                    .get_mut(None)
                    .canvas
                    .scene = Scene::new();
                self.render_canvas(ui, world);
                return;
            };
            if world.resource::<WorkbenchState>().open_model.is_none() {
                self.render_canvas(ui, world);
                return;
            }
            let gen = world
                .resource::<ModelicaDocumentRegistry>()
                .host(doc_id)
                .map(|h| h.document().generation())
                .unwrap_or(0);
            let state = world.resource::<CanvasDiagramState>();
            // Two triggers for projection:
            //   1. **First render of this tab** — `has_entry` is
            //      false. MSL library docs land with generation 0
            //      and our fresh state cursor also starts at 0, so a
            //      gen-only check would never fire and the drilled-
            //      in canvas would stay blank forever. Insert-on-
            //      first-render is the right fix.
            //   2. **Doc mutated** — generation bumped past
            //      `last_seen_gen`. Standard edit-reproject path.
            let docstate = state.get(Some(doc_id));
            let first_render = !state.has_entry(doc_id);
            // `gen_advanced` is the source-of-truth-changed signal,
            // but a canvas-originated edit (drag, menu Add) has
            // already mutated the scene and bumped
            // `canvas_acked_gen` to the new doc generation — in that
            // case the projection would just rebuild what's already
            // on screen, so we suppress it. Foreign edits (typed in
            // the code editor) bump `gen` past `canvas_acked_gen` and
            // this filter passes through unchanged.
            let gen_advanced =
                gen != docstate.last_seen_gen && gen > docstate.canvas_acked_gen;
            // Drill-in target changed (e.g. user clicked a different
            // class in the Twin Browser for an already-open tab).
            let live_target = world
                .get_resource::<DrilledInClassNames>()
                .and_then(|m| m.get(doc_id).map(str::to_string));
            let target_changed = live_target != docstate.last_seen_target;
            // Hash-skip: when the gen bumped but the projection-
            // relevant source slice (whitespace-collapsed, comment-
            // stripped) is unchanged, mark the gen as seen and bail
            // out without spawning a projection task. This is the
            // cheap layer of "intelligent reprojection" — catches
            // comment / blank-line / parameter-default edits before
            // they pay the rumoca-projection cost.
            // Block projection while the AST is stale. The parse runs
            // off-thread (cf. `crate::ui::ast_refresh`) and takes a
            // couple of seconds in debug builds; if we project against
            // the *previous* AST, the new structural change is invisible
            // and `last_seen_gen` advances anyway — so the next AST
            // refresh produces no follow-up reproject and the user
            // stares at a stale scene forever. We disable
            // `needs_project` here so we still fall through to the
            // polling block below (resolves any in-flight task) but
            // don't spawn a new one.
            let ast_stale_for_doc = world
                .resource::<ModelicaDocumentRegistry>()
                .host(doc_id)
                .map(|h| h.document().ast_is_stale())
                .unwrap_or(false);
            if ast_stale_for_doc {
                // Keep egui awake so the next frame re-checks once
                // the off-thread parse has landed.
                ui.ctx().request_repaint();
            }

            let needs_project = !ast_stale_for_doc && (first_render || target_changed || {
                if !gen_advanced {
                    false
                } else {
                    let new_hash = world
                        .resource::<ModelicaDocumentRegistry>()
                        .host(doc_id)
                        .map(|h| projection_relevant_source_hash(h.document().source()))
                        .unwrap_or(0);
                    if new_hash == docstate.last_seen_source_hash {
                        // Mark the gen as seen so the render loop
                        // doesn't keep re-checking every frame.
                        // Drop the read-only borrow first.
                        drop(state);
                        let mut state =
                            world.resource_mut::<CanvasDiagramState>();
                        let docstate = state.get_mut(Some(doc_id));
                        docstate.last_seen_gen = gen;
                        bevy::log::debug!(
                            "[CanvasDiagram] skipping reproject for gen={gen} (source-hash unchanged)"
                        );
                        false
                    } else {
                        true
                    }
                }
            });
            needs_project.then_some((doc_id, gen))
        };

        if let Some((doc_id, gen)) = project_now {
            // Spawn a background task (or reuse an in-flight one
            // for the same doc+gen) that runs edge-recovery and
            // builds a `Scene` from the document's already-parsed
            // AST — no re-parse. Hot path: clone the `Arc<StoredDefinition>`
            // (cheap) + clone the source (byte copy) and ship both
            // to the task. `import_model_to_diagram_from_ast` avoids
            // the two full rumoca passes `import_model_to_diagram`
            // used to run.
            let (source, ast_arc) = {
                let registry = world.resource::<ModelicaDocumentRegistry>();
                let Some(host) = registry.host(doc_id) else {
                    // Doc reserved but not yet installed (duplicate /
                    // drill-in still parsing). Skip projection but
                    // STILL paint the canvas so the loading overlay
                    // shows. The earlier early-returns at the top of
                    // Panel::render are careful to do this; this one
                    // forgot — without the call below the entire
                    // bg-parse window paints nothing.
                    drop(registry);
                    self.render_canvas(ui, world);
                    return;
                };
                let doc = host.document();
                // Prefer the strict AST (`doc.ast()`) when it parsed
                // cleanly — `parse_to_ast` and `parse_to_syntax`
                // produce subtly different trees that the diagram
                // builder isn't fully tolerant of (observed: 0-node
                // canvas after duplicating a working example when the
                // lenient tree was used). Fall through to the lenient
                // `SyntaxCache` only when strict parse failed — that
                // way partial-parse states still draw something
                // instead of going blank.
                let ast = match doc.ast().result.as_ref().ok() {
                    Some(strict) => std::sync::Arc::clone(strict),
                    None => std::sync::Arc::clone(&doc.syntax_arc().ast),
                };
                (doc.source().to_string(), ast)
            };
            // Snapshot the configurable projection caps so the bg
            // task doesn't need to reach back into the world (it
            // can't — it runs off-thread with only owned data).
            let (max_nodes_snapshot, max_duration_snapshot) = world
                .get_resource::<DiagramProjectionLimits>()
                .map(|l| (l.max_nodes, l.max_duration))
                .unwrap_or((
                    crate::ui::panels::canvas_projection::DEFAULT_MAX_DIAGRAM_NODES,
                    std::time::Duration::from_secs(60),
                ));
            // Target class for the projection: the fully-qualified
            // name the drill-in tab points at. Read from
            // `DrilledInClassNames`, which the drill-in install
            // populated and which persists for the tab's lifetime.
            // Reading `open_model.model_path` doesn't work here —
            // for installed docs it's the filesystem path, not the
            // `msl://…` URI. `None` for Untitled / user-authored
            // docs — builder picks the first non-package class as
            // before.
            let target_class_snapshot: Option<String> = world
                .get_resource::<DrilledInClassNames>()
                .and_then(|m| m.get(doc_id).map(str::to_string));
            // Snapshot the auto-layout grid so the bg task can fall
            // back to configurable spacing for components without a
            // `Placement` annotation.
            let layout_snapshot = world
                .get_resource::<crate::ui::panels::canvas_projection::DiagramAutoLayoutSettings>()
                .cloned()
                .unwrap_or_default();
            let mut state = world.resource_mut::<CanvasDiagramState>();
            let docstate = state.get_mut(Some(doc_id));
            // If the user just changed drill-in target (clicked a
            // different class in the Twin Browser), the new scene's
            // bounds usually have nothing to do with the old one —
            // so we want auto-fit to engage exactly as it does on a
            // fresh tab open. Resetting `last_seen_gen` to 0 makes
            // the completion path treat this projection as
            // "initial" and refit the viewport, instead of leaving
            // the camera at the stale zoom that made the new icons
            // look "way too far apart" (icons rendered at near 1:1
            // because the previous package-level scene auto-fit
            // happened at a different scale).
            if docstate.last_seen_target != target_class_snapshot {
                docstate.last_seen_gen = 0;
            }
            // Refresh the Diagram-annotation background decoration
            // for the target class. Cheap AST walk (no re-parse —
            // `ast_arc` is the already-parsed tree the task below
            // consumes). Runs on main thread; `paint_graphics` is
            // idle until the layer's next draw.
            let bg_handle = docstate.background_diagram.clone();
            let diag = diagram_annotation_for_target(
                ast_arc.as_ref(),
                target_class_snapshot.as_deref(),
            );
            if let Ok(mut guard) = bg_handle.write() {
                // Stash the *full* graphics list so the projection
                // completion handler can pick out `Text` and
                // `LunCoPlotNode` items and emit corresponding
                // scene Nodes. The decoration painter (which only
                // wants the static-decoration subset) filters Text
                // and LunCoPlotNode out itself in `Layer::draw`.
                *guard = diag.map(|d| (d.coordinate_system, d.graphics));
            }
            // Drop any in-flight projection whose input is now
            // stale (older generation of this doc). We can't cancel
            // a `Task` cleanly in Bevy's API, but dropping the
            // handle releases our interest — the pool still runs it
            // to completion, the result is just thrown away when we
            // poll. Cross-doc staleness is no longer possible now
            // that tasks live on per-doc state.
            let stale = match &docstate.projection_task {
                Some(t) => t.gen_at_spawn != gen,
                None => false,
            };
            if stale {
                docstate.projection_task = None;
            }
            if docstate.projection_task.is_none() {
                // Hard ceiling: the projection path is now
                // `Arc<StoredDefinition>`-based (no deep clone), so
                // MB-scale ASTs are no longer an OOM risk. The cap
                // below is a never-freeze guarantee against pathological
                // inputs (gigabyte sources, etc.) — not a routine
                // throttle. Tune only if users actually hit it. The
                // per-class graph cap (`DiagramProjectionLimits::max_nodes`,
                // user-configurable) catches the "this file parsed
                // fine but has too many components to show usefully"
                // case.
                const PROJECTION_SOURCE_HARD_CEILING: usize = 10_000_000; // 10 MB
                let source_len = source.len();
                let skip_projection = source_len > PROJECTION_SOURCE_HARD_CEILING;
                if skip_projection {
                    bevy::log::warn!(
                        "[CanvasDiagram] refusing to project: source {} bytes \
                         exceeds the {} hard ceiling. Use Text view.",
                        source_len,
                        PROJECTION_SOURCE_HARD_CEILING,
                    );
                    // Mark as "seen at this gen" so the render loop
                    // doesn't keep retrying every frame.
                    docstate.last_seen_gen = gen;
                } else {
                    let pool = bevy::tasks::AsyncComputeTaskPool::get();
                    let spawned_at = web_time::Instant::now();
                    let cancel = std::sync::Arc::new(
                        std::sync::atomic::AtomicBool::new(false),
                    );
                    let cancel_for_task = std::sync::Arc::clone(&cancel);
                    let deadline = max_duration_snapshot;
                    let target_for_log = target_class_snapshot.clone();
                    let source_bytes_for_log = source.len();
                    // Compute the hash now (off the move into the task)
                    // so the completion handler can stash it on the
                    // docstate without re-fetching the source.
                    let source_hash_at_spawn =
                        projection_relevant_source_hash(&source);
                    let task = pool.spawn(async move {
                        use std::sync::atomic::Ordering;
                        let should_stop = || {
                            cancel_for_task.load(Ordering::Relaxed)
                                || spawned_at.elapsed() > deadline
                        };
                        bevy::log::info!(
                            "[Projection] start: {} bytes target={:?}",
                            source_bytes_for_log,
                            target_for_log,
                        );
                        if should_stop() {
                            return Scene::new();
                        }
                        let t0 = web_time::Instant::now();
                        let mut diagram =
                            crate::ui::panels::canvas_projection::import_model_to_diagram_from_ast(
                                ast_arc,
                                &source,
                                max_nodes_snapshot,
                                target_for_log.as_deref(),
                                &layout_snapshot,
                            )
                            .unwrap_or_default();
                        bevy::log::info!(
                            "[Projection] import done in {:.0}ms: {} nodes {} edges",
                            t0.elapsed().as_secs_f64() * 1000.0,
                            diagram.nodes.len(),
                            diagram.edges.len(),
                        );
                        if should_stop() {
                            return Scene::new();
                        }
                        let t1 = web_time::Instant::now();
                        recover_edges_from_source(&source, &mut diagram);
                        bevy::log::info!(
                            "[Projection] recover_edges done in {:.0}ms: {} edges",
                            t1.elapsed().as_secs_f64() * 1000.0,
                            diagram.edges.len(),
                        );
                        if should_stop() {
                            return Scene::new();
                        }
                        let t2 = web_time::Instant::now();
                        let (scene, _id_map) = project_scene(&diagram);
                        bevy::log::info!(
                            "[Projection] project_scene done in {:.0}ms",
                            t2.elapsed().as_secs_f64() * 1000.0,
                        );
                        scene
                    });
                    docstate.projection_task = Some(ProjectionTask {
                        gen_at_spawn: gen,
                        target_at_spawn: target_class_snapshot.clone(),
                        spawned_at,
                        deadline,
                        cancel,
                        task,
                        source_hash: source_hash_at_spawn,
                    });
                }
            }
            // DO NOT update last_seen_gen here — only after the
            // task completes and the scene is actually swapped in.
            // Otherwise the `project_now` check on later frames
            // would think we're up-to-date and never swap.
            let _ = state;
        }

        // Poll the in-flight projection task for the ACTIVE doc.
        // When it finishes, swap the scene in, update the sync
        // cursor, and (on first projection for this tab) frame the
        // scene with a sensible initial zoom.
        {
            let active_doc = world
                .resource::<lunco_workbench::WorkspaceResource>()
                .active_document;
            // Pre-fetch current gen from the registry before we
            // take the mutable borrow of CanvasDiagramState, so we
            // can use it inside the deadline-guard block below
            // without fighting borrow rules.
            let current_gen_for_deadline = active_doc.and_then(|d| {
                world
                    .get_resource::<ModelicaDocumentRegistry>()
                    .and_then(|r| r.host(d))
                    .map(|h| h.document().generation())
            });
            let mut state = world.resource_mut::<CanvasDiagramState>();
            let docstate = state.get_mut(active_doc);
            let is_initial_projection = docstate.last_seen_gen == 0;

            // Deadline guard. If the task has been running past its
            // configured budget, flip its cancel flag and drop the
            // handle. The pool still runs the task to completion
            // (Bevy tasks can't be preempted), but the cooperative
            // `should_stop` check inside the task short-circuits
            // the remaining phases and nobody waits on the result.
            // We mark `last_seen_gen = current_gen` so the render
            // loop doesn't respawn the same doomed task next frame;
            // the user has to edit the doc (generation bump) to
            // retry — which is the correct recovery action.
            let timed_out = docstate
                .projection_task
                .as_ref()
                .map(|t| t.spawned_at.elapsed() > t.deadline)
                .unwrap_or(false);
            if timed_out {
                use std::sync::atomic::Ordering;
                if let Some(t) = docstate.projection_task.as_ref() {
                    t.cancel.store(true, Ordering::Relaxed);
                    bevy::log::warn!(
                        "[CanvasDiagram] projection exceeded {:.1}s deadline \
                         — cancelled. Raise Settings → Diagram → Timeout \
                         to allow longer.",
                        t.deadline.as_secs_f32(),
                    );
                }
                docstate.projection_task = None;
                if let Some(g) = current_gen_for_deadline {
                    docstate.last_seen_gen = g;
                }
            }

            let done_task = docstate
                .projection_task
                .as_mut()
                .and_then(|t| {
                    futures_lite::future::block_on(
                        futures_lite::future::poll_once(&mut t.task),
                    )
                    .map(|scene| {
                        (
                            t.gen_at_spawn,
                            t.target_at_spawn.clone(),
                            t.source_hash,
                            scene,
                        )
                    })
                });
            // Drop the projection result if the doc has moved on
            // while it was running. Projection tasks can take tens
            // of seconds when MSL `extends`-chain resolution misses
            // the cache (each miss does a sync rumoca parse inside
            // the task — see `peek_or_load_msl_class`). During that
            // window the user may have added several components via
            // the optimistic-synth path; swapping in the stale
            // projected scene wipes those nodes. We KEEP the
            // optimistic scene as-is; the next projection (gated by
            // `gen_advanced` on a fresher source) will reconcile.
            let done_task = done_task.and_then(|(gen, target, source_hash, scene)| {
                if gen < docstate.canvas_acked_gen {
                    bevy::log::info!(
                        "[CanvasDiagram] discarding stale projection: \
                         project_gen={gen} canvas_acked_gen={} \
                         ({} nodes, {} edges)",
                        docstate.canvas_acked_gen,
                        scene.node_count(),
                        scene.edge_count(),
                    );
                    docstate.projection_task = None;
                    None
                } else {
                    Some((gen, target, source_hash, scene))
                }
            });
            if let Some((gen, target, source_hash, scene)) = done_task {
                docstate.projection_task = None;
                // Bug guard: if the new scene is empty but the existing
                // scene had content, the user almost certainly hit a
                // transient parse failure (mid-edit, malformed annotation,
                // etc.). Keep the last good render rather than blanking
                // the canvas. The next successful parse will swap a
                // populated scene back in.
                if scene.node_count() == 0 && docstate.canvas.scene.node_count() > 0 {
                    bevy::log::info!(
                        "[CanvasDiagram] dropping empty projection — keeping last good scene ({} nodes)",
                        docstate.canvas.scene.node_count(),
                    );
                    docstate.last_seen_gen = gen;
                    docstate.last_seen_target = target;
                    docstate.last_seen_source_hash = source_hash;
                    return;
                }
                bevy::log::info!(
                    "[CanvasDiagram] project done: {} nodes, {} edges (initial={})",
                    scene.node_count(),
                    scene.edge_count(),
                    is_initial_projection,
                );
                // Preserve the user's selection across re-projection
                // when the same node is still in the new scene — the
                // prior unconditional `clear()` made every drag /
                // small edit feel like a visual reset (the highlight
                // ring would briefly vanish after each SetPlacement
                // cycle). We match nodes by `origin` (= the Modelica
                // instance name) since Bevy IDs change across scene
                // rebuilds.
                let preserved_origins: std::collections::HashSet<String> = docstate
                    .canvas
                    .selection
                    .iter()
                    .filter_map(|sid| match sid {
                        lunco_canvas::SelectItem::Node(nid) => docstate
                            .canvas
                            .scene
                            .node(*nid)
                            .and_then(|n| n.origin.clone()),
                        _ => None,
                    })
                    .collect();
                // Build an old-id → new-id map keyed by node origin
                // (Modelica instance name) so we can remap any in-
                // flight tool gesture (press / drag / connect)
                // across the wholesale scene swap. The pre-existing
                // symptom users saw was "first click+drag does
                // nothing, second one works": the press registered
                // against a NodeId from the old scene, the move
                // handler couldn't promote to a drag because
                // `scene.node(id)` returned None for the stale id.
                // Remapping by origin preserves the gesture across
                // re-projection so the first attempt completes.
                let old_origin_to_id: std::collections::HashMap<String, lunco_canvas::NodeId> =
                    docstate
                        .canvas
                        .scene
                        .nodes()
                        .filter_map(|(id, n)| n.origin.clone().map(|o| (o, *id)))
                        .collect();
                let new_origin_to_id: std::collections::HashMap<String, lunco_canvas::NodeId> =
                    scene
                        .nodes()
                        .filter_map(|(id, n)| n.origin.clone().map(|o| (o, *id)))
                        .collect();
                let id_remap: std::collections::HashMap<lunco_canvas::NodeId, lunco_canvas::NodeId> =
                    old_origin_to_id
                        .iter()
                        .filter_map(|(origin, old_id)| {
                            new_origin_to_id.get(origin).map(|new_id| (*old_id, *new_id))
                        })
                        .collect();
                docstate.canvas.tool.remap_node_ids(&|old: lunco_canvas::NodeId| {
                    id_remap.get(&old).copied()
                });
                // Plot tiles are persisted in the diagram annotation
                // as `__LunCo_PlotNode(extent=…, signal=…, title=…)`
                // vendor entries. Re-emit one scene Node per
                // annotation so they reload with the document.
                // Scene-only plot nodes (added via the runtime
                // "Add plot" gesture but not yet round-tripped to
                // source) carry no `origin` marker — those still
                // get preserved via the legacy carry-over below so
                // adding a plot doesn't blink off-screen between
                // the gesture and the next save.
                let mut scene = scene;
                let bg_graphics: Vec<crate::annotations::GraphicItem> = docstate
                    .background_diagram
                    .read()
                    .ok()
                    .and_then(|g| g.as_ref().map(|(_, gfx)| gfx.clone()))
                    .unwrap_or_default();
                let plot_origins_from_source =
                    emit_diagram_decorations(&mut scene, &bg_graphics);
                // Legacy carry-over: scratch plot nodes from a prior
                // scene that haven't been written to source yet.
                // Filtered against `plot_origins_from_source` so we
                // don't double-insert a plot the source already
                // describes.
                const SCENE_ONLY_KINDS: &[&str] = &[
                    lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND,
                ];
                let scene_only_nodes: Vec<lunco_canvas::scene::Node> = docstate
                    .canvas
                    .scene
                    .nodes()
                    .filter(|(_, n)| {
                        SCENE_ONLY_KINDS.contains(&n.kind.as_str())
                            && n.origin
                                .as_deref()
                                .map(|o| !plot_origins_from_source.contains(o))
                                .unwrap_or(true)
                    })
                    .map(|(_, n)| n.clone())
                    .collect();
                for mut node in scene_only_nodes {
                    node.id = scene.alloc_node_id();
                    scene.insert_node(node);
                }
                docstate.canvas.scene = scene;
                docstate.canvas.selection.clear();
                if !preserved_origins.is_empty() {
                    let new_ids: Vec<lunco_canvas::NodeId> = docstate
                        .canvas
                        .scene
                        .nodes()
                        .filter_map(|(nid, n)| {
                            n.origin
                                .as_deref()
                                .filter(|o| preserved_origins.contains(*o))
                                .map(|_| *nid)
                        })
                        .collect();
                    for id in new_ids {
                        docstate.canvas.selection.add(lunco_canvas::SelectItem::Node(id));
                    }
                }
                docstate.last_seen_gen = gen;
                docstate.last_seen_target = target;
                // Cache the projection-relevant source hash that the
                // task captured at spawn time. Next frame's
                // gen-advanced check skips reprojection when the
                // current source hashes to the same value (comment /
                // whitespace edit). Best-effort: if a newer edit
                // landed mid-projection, the hash will differ from
                // current source — gen-advanced check will then
                // trigger the follow-up projection, correct.
                docstate.last_seen_source_hash = source_hash;
                if is_initial_projection {
                    let physical_zoom =
                        lunco_canvas::Viewport::physical_mm_zoom(ui.ctx());
                    if let Some(world_rect) = docstate.canvas.scene.bounds() {
                        let screen = lunco_canvas::Rect::from_min_max(
                            lunco_canvas::Pos::new(0.0, 0.0),
                            lunco_canvas::Pos::new(800.0, 600.0),
                        );
                        let (c, z) = docstate
                            .canvas
                            .viewport
                            .fit_values(world_rect, screen, 40.0);
                        let z = z.min(physical_zoom * 2.0).max(physical_zoom * 0.5);
                        docstate.canvas.viewport.snap_to(c, z);
                    } else {
                        docstate.canvas.viewport.snap_to(
                            lunco_canvas::Pos::new(0.0, 0.0),
                            physical_zoom,
                        );
                    }
                }
                // A projection just finished — request a repaint so
                // the user sees the new scene immediately rather
                // than on the next input tick.
                ui.ctx().request_repaint();
            } else if docstate.projection_task.is_some() {
                // Still parsing — repaint so the "Projecting…"
                // indicator animates smoothly.
                ui.ctx().request_repaint();
            }
        }

        let t_render_canvas = web_time::Instant::now();
        self.render_canvas(ui, world);
        let render_canvas_ms = t_render_canvas.elapsed().as_secs_f64() * 1000.0;
        let total_ms = _frame_t0.elapsed().as_secs_f64() * 1000.0;
        // Post-Add window tracker: every frame after a recent
        // apply_ops gets logged regardless of how fast it was, so we
        // see exactly what the user is feeling. Hooked off the
        // global wall-clock timestamp stamped by `apply_ops` (see
        // `LAST_APPLY_AT`); 2-second window after the last apply.
        let mut force_log = false;
        if let Ok(guard) = LAST_APPLY_AT.lock() {
            if let Some(t) = *guard {
                if t.elapsed().as_secs_f64() < 2.0 {
                    force_log = true;
                }
            }
        }
        // 8ms threshold — half a 60 Hz frame; anything beyond this
        // is enough to cause a visible animation hiccup on a
        // 120 Hz monitor.
        if total_ms > 8.0 || force_log {
            bevy::log::info!(
                "[CanvasDiagram] frame: total={total_ms:.1}ms render_canvas={render_canvas_ms:.1}ms{}",
                if force_log { " (post-apply window)" } else { "" }
            );
        }
    }
}

/// Wall-clock timestamp of the most recent `apply_ops` call. Used
/// by the post-Add window tracker in the panel render to log every
/// frame for ~2 seconds after each Add — captures sub-threshold
/// hitches that don't trip the SLOW frame log on their own but add
/// up to a perceived "freeze" when the user does something.
static LAST_APPLY_AT: std::sync::Mutex<Option<web_time::Instant>> =
    std::sync::Mutex::new(None);

impl CanvasDiagramPanel {
    fn render_canvas(&self, ui: &mut egui::Ui, world: &mut World) {
        // Per-phase timing harness — gated on `RENDER_CANVAS_TRACE`
        // env var so the SLOW-frame log can pinpoint the heavy phase
        // without flooding normal runs. Set the var to anything
        // non-empty (`RENDER_CANVAS_TRACE=1`) to enable.
        let trace_phases = std::env::var_os("RENDER_CANVAS_TRACE").is_some();
        let mut phase_t = web_time::Instant::now();
        let mut phase_log: Vec<(&'static str, f64)> = Vec::new();
        let mut mark = |label: &'static str, t: &mut web_time::Instant, log: &mut Vec<(&'static str, f64)>| {
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if ms > 1.0 {
                log.push((label, ms));
            }
            *t = web_time::Instant::now();
        };

        // Resolve editing class + doc id up front. These drive op
        // emission; without them (no doc bound, or unparseable
        // source) the canvas stays read-only — events still fire
        // but translate to nothing, matching "no-op on empty doc".
        let (doc_id, editing_class) = resolve_doc_context(world);
        mark("resolve_doc_context", &mut phase_t, &mut phase_log);

        // Active doc — the tab whose canvas should respond to
        // input this frame. All state accesses below route through
        // this id so neighbour tabs stay untouched.
        let active_doc = doc_id;

        // Read-only library class (MSL, imported file the user
        // opened via drill-in) — no editing gestures should take
        // effect here. We gate the whole right-click menu on this
        // so readonly tabs don't even offer "Add component" etc.;
        // the canvas itself stays fully navigable (pan/zoom/select).
        let tab_read_only = world
            .resource::<WorkbenchState>()
            .open_model
            .as_ref()
            .map(|m| m.read_only)
            .unwrap_or(false);

        // Render the canvas and collect its events. Flip the
        // canvas's `read_only` flag so the tool layer refuses to
        // enter drag/connect/delete states — pan + zoom + selection
        // still work. Authored scene mutations are blocked at the
        // input source, not corrected after the fact.
        // Snap settings come from a long-lived resource that the
        // Settings menu toggles. Read it here each frame and push
        // onto the canvas so the tool sees an up-to-date value
        // during the next drag update. Off by default — users turn
        // it on when they want drag alignment.
        let snap_settings: Option<lunco_canvas::SnapSettings> = world
            .get_resource::<CanvasSnapSettings>()
            .filter(|s| s.enabled)
            .map(|s| lunco_canvas::SnapSettings { step: s.step });

        // Theme snapshot: computed once per render and stashed in the
        // egui context so the NodeVisual / EdgeVisual trait objects
        // inside `canvas.ui` (which have no `World` access) can still
        // pick theme-aware colours on draw.
        {
            let theme = world
                .get_resource::<lunco_theme::Theme>()
                .cloned()
                .unwrap_or_else(lunco_theme::Theme::dark);
            store_canvas_theme(
                ui.ctx(),
                CanvasThemeSnapshot::from_theme(&theme),
            );
            store_modelica_icon_palette(ui.ctx(), theme.modelica_icons.clone());
            lunco_canvas::theme::store(
                ui.ctx(),
                layer_theme_from(&theme),
            );
        }

        // Stash a per-frame snapshot of `SignalRegistry` data so any
        // `lunco.viz.plot` scene nodes drawn this frame can read live
        // samples without a `World` reference. Visuals live in
        // `lunco-viz` and have no Bevy access; the snapshot is the
        // bridge. Empty when no SignalRegistry is installed —
        // `PlotNodeVisual` degrades to "title only".
        if let Some(sig_reg) = world.get_resource::<lunco_viz::SignalRegistry>() {
            let mut snapshot =
                lunco_viz::kinds::canvas_plot_node::SignalSnapshot::default();
            for (sig_ref, hist) in sig_reg.iter_scalar() {
                let pts: Vec<[f64; 2]> =
                    hist.samples.iter().map(|s| [s.time, s.value]).collect();
                snapshot
                    .samples
                    .insert((sig_ref.entity, sig_ref.path.clone()), pts);
            }
            lunco_viz::kinds::canvas_plot_node::stash_signal_snapshot(
                ui.ctx(),
                snapshot,
            );
        }

        // Stash a flat per-instance value snapshot so node visuals
        // (icon hover tooltips, future inline value badges, etc.)
        // can read parameters / inputs / live variables without
        // touching the World. Combines all three buckets keyed by
        // dotted instance path (`R1.R`, `P.y`, …); visuals filter by
        // the prefix that matches their instance name.
        // Resolve THIS canvas's simulator entity once. Doc-scoped, not
        // active-tab-scoped: a future split-pane / multi-canvas layout
        // where two model views are visible at once stays correct
        // because each canvas reads from `simulator_for(world, its
        // own doc_id)`. The `query.iter().next()` pattern that lived
        // here previously picked an arbitrary `ModelicaModel` from
        // the world — fine with one model loaded, catastrophically
        // wrong with two (parameter/input snapshots and slider writes
        // would land on whichever entity Bevy iterated first).
        let canvas_sim = doc_id.and_then(|d| {
            crate::ui::state::simulator_for(world, d)
        });

        // Re-bind every embedded plot tile's `entity` to the live
        // simulator each frame. Source-emitted plot Nodes are
        // serialised with `entity = 0` (Modelica annotations don't
        // know about Bevy entity ids; they'd be meaningless across
        // sessions anyway) — without this re-bind the
        // `SignalSnapshot.samples[(entity, path)]` lookup misses
        // and the chart stays empty even while the sim publishes
        // history. Cheap loop: typical canvas has < 10 plot
        // tiles, and we only mutate when the entity actually
        // changed.
        if let (Some(d), Some(sim)) = (doc_id, canvas_sim) {
            let new_bits = sim.to_bits();
            let mut state = world.resource_mut::<CanvasDiagramState>();
            let docstate = state.get_mut(Some(d));
            let plot_ids: Vec<lunco_canvas::NodeId> = docstate
                .canvas
                .scene
                .nodes()
                .filter(|(_, n)| {
                    n.kind == lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND
                })
                .map(|(id, _)| *id)
                .collect();
            for id in plot_ids {
                let Some(node) = docstate.canvas.scene.node_mut(id) else { continue };
                let Some(prev) = node
                    .data
                    .downcast_ref::<lunco_viz::kinds::canvas_plot_node::PlotNodeData>()
                    .cloned()
                else {
                    continue;
                };
                if prev.entity != new_bits {
                    let updated = lunco_viz::kinds::canvas_plot_node::PlotNodeData {
                        entity: new_bits,
                        ..prev
                    };
                    node.data = std::sync::Arc::new(updated);
                }
            }
        }

        {
            let mut state =
                lunco_viz::kinds::canvas_plot_node::NodeStateSnapshot::default();
            if let Some(entity) = canvas_sim {
                if let Some(model) = world.get::<crate::ModelicaModel>(entity) {
                    for (k, v) in &model.parameters {
                        state.values.insert(k.clone(), *v);
                    }
                    for (k, v) in &model.inputs {
                        state.values.insert(k.clone(), *v);
                    }
                    for (k, v) in &model.variables {
                        state.values.insert(k.clone(), *v);
                    }
                }
            }
            lunco_viz::kinds::canvas_plot_node::stash_node_state(
                ui.ctx(),
                state,
            );
            // Flow-animation time base. Advances by the current
            // frame's dt only while the simulator is stepping, so
            // dots freeze on pause (staying visible at their last
            // position) and resume from the same phase on unpause.
            // Edge visuals read this via `fetch_flow_anim_time()`.
            // Scoped to THIS canvas's sim — animation stops when
            // this doc's model is paused, regardless of whether
            // another tab's sim is still running.
            let any_unpaused = canvas_sim
                .and_then(|e| world.get::<crate::ModelicaModel>(e))
                .map(|m| !m.paused)
                .unwrap_or(false);
            let dt = ui.ctx().input(|i| i.stable_dt as f64);
            let prev = ui
                .ctx()
                .data(|d| {
                    d.get_temp::<f64>(egui::Id::new("lunco_modelica_flow_anim_time"))
                })
                .unwrap_or(0.0);
            let next = if any_unpaused { prev + dt } else { prev };
            ui.ctx().data_mut(|d| {
                d.insert_temp(
                    egui::Id::new("lunco_modelica_flow_anim_time"),
                    next,
                );
                d.insert_temp(
                    egui::Id::new("lunco_modelica_sim_stepping"),
                    any_unpaused,
                );
            });
        }

        // Stash the input-control snapshot so each canvas icon can
        // render an in-canvas control widget bound to its own
        // RealInput(s). Includes the input's current value plus
        // declared min/max bounds (with the same leaf-name fallback
        // Telemetry uses, since `parameter_bounds` keys by leaf
        // component name and the runtime queries by qualified path).
        // Publishing this every frame keeps the widgets responsive
        // to recompiles, parameter changes, and external writes.
        {
            let mut control_snapshot =
                lunco_viz::kinds::canvas_plot_node::InputControlSnapshot::default();
            if let Some(entity) = canvas_sim {
                if let Some(model) = world.get::<crate::ModelicaModel>(entity) {
                    for (qualified, value) in &model.inputs {
                        let leaf = qualified.rsplit('.').next().unwrap_or(qualified);
                        let (mn, mx) = model
                            .parameter_bounds
                            .get(qualified)
                            .copied()
                            .or_else(|| model.parameter_bounds.get(leaf).copied())
                            .unwrap_or((None, None));
                        control_snapshot
                            .inputs
                            .insert(qualified.clone(), (*value, mn, mx));
                    }
                }
            }
            lunco_viz::kinds::canvas_plot_node::stash_input_control_snapshot(
                ui.ctx(),
                control_snapshot,
            );
        }

        mark("snapshots+sigreg", &mut phase_t, &mut phase_log);

        let (response, events) = {
            let mut state = world.resource_mut::<CanvasDiagramState>();
            let docstate = state.get_mut(active_doc);
            docstate.canvas.read_only = tab_read_only;
            docstate.canvas.snap = snap_settings;
            docstate.canvas.ui(ui)
        };
        mark("canvas.ui (scene render)", &mut phase_t, &mut phase_log);

        // Vello continues to render the diagram in the background
        // into a per-tab offscreen texture (see `vello_canvas.rs`).
        // The composite back into the panel is intentionally not
        // wired right now — vello text rendering with offscreen
        // RenderTarget::Image targets is buggy in bevy_vello 0.13.1
        // (entities spawn + extract correctly but the text glyphs
        // never appear in the image). Until that's resolved, the
        // egui canvas continues to paint the diagram for users; the
        // vello pipeline is exercised end-to-end for everything
        // EXCEPT text + bitmap and stays ready for re-enabling once
        // the upstream fix lands.

        // ── Palette drag-and-drop drop handler ──
        //
        // Source side (palette row) sets a `ComponentDragPayload` on
        // drag-start. Here we (a) draw a ghost preview at the cursor
        // while the payload is live and the cursor is over our canvas
        // rect, and (b) on pointer release commit the drop: if over
        // the canvas, fire `AddModelicaComponent` at cursor coords
        // (Modelica convention, +Y up); if not over the canvas, just
        // clear the payload so a missed drop doesn't leave stale
        // state. Read-only tabs ignore the drop entirely (consistent
        // with the right-click-menu gating).
        let drag_payload_def = world
            .get_resource::<crate::ui::panels::palette::ComponentDragPayload>()
            .and_then(|p| p.def.clone());
        if let Some(def) = drag_payload_def {
            let hover_pos = response.hover_pos();
            // Ghost preview: a translucent rectangle at the cursor,
            // sized to match the default 20-unit Modelica icon as
            // rendered by the current viewport zoom. Falls back to
            // a fixed pixel size if zoom is unreadable.
            if let Some(p) = hover_pos {
                let painter = ui.painter_at(response.rect);
                // Translate ICON_W/H (modelica units) to screen px
                // via the canvas viewport zoom for size accuracy.
                let zoom = world
                    .resource::<CanvasDiagramState>()
                    .get(active_doc)
                    .canvas
                    .viewport
                    .zoom;
                let half = (ICON_W * zoom * 0.5).max(12.0);
                let ghost_rect = egui::Rect::from_center_size(
                    p,
                    egui::vec2(half * 2.0, half * 2.0),
                );
                let accent = egui::Color32::from_rgb(120, 180, 255);
                painter.rect_filled(
                    ghost_rect,
                    4.0,
                    egui::Color32::from_rgba_unmultiplied(120, 180, 255, 50),
                );
                painter.rect_stroke(
                    ghost_rect,
                    4.0,
                    egui::Stroke::new(1.5, accent),
                    egui::StrokeKind::Outside,
                );
                painter.text(
                    egui::pos2(ghost_rect.center().x, ghost_rect.max.y + 4.0),
                    egui::Align2::CENTER_TOP,
                    &def.display_name,
                    egui::FontId::proportional(11.0),
                    accent,
                );
                ui.ctx().request_repaint();
            }

            // Commit on pointer release. Global input — fires whether
            // the release happened over us or elsewhere; we discriminate
            // by `hover_pos` being set + within our rect.
            let released = ui.input(|i| i.pointer.any_released());
            if released {
                let drop_target = hover_pos.filter(|p| response.rect.contains(*p));
                if let (Some(p), Some(doc_id)) = (drop_target, active_doc) {
                    if !tab_read_only {
                        // Match the right-click "Add component" path
                        // exactly: optimistic `synthesize_msl_node` for
                        // instant visual response + `apply_ops_public`
                        // to rewrite the source and bump
                        // `canvas_acked_gen` so the eventual reproject
                        // is suppressed (which is why the existing
                        // scene survives — the API-observer path went
                        // through `AddModelicaComponent`, which had no
                        // such ack, so the canvas kept clearing itself
                        // and stalled on the 2.5 s reparse debounce).
                        let screen_rect_drop = lunco_canvas::Rect::from_min_max(
                            lunco_canvas::Pos::new(response.rect.min.x, response.rect.min.y),
                            lunco_canvas::Pos::new(response.rect.max.x, response.rect.max.y),
                        );
                        let click_world = world
                            .resource::<CanvasDiagramState>()
                            .get(active_doc)
                            .canvas
                            .viewport
                            .screen_to_world(
                                lunco_canvas::Pos::new(p.x, p.y),
                                screen_rect_drop,
                            );
                        // Resolve target class — drilled-in if present,
                        // else extracted from the doc's first non-package
                        // class. Same fallback the palette click uses.
                        let class = editing_class.clone().unwrap_or_else(|| {
                            world
                                .get_resource::<DrilledInClassNames>()
                                .and_then(|m| m.get(doc_id).map(str::to_string))
                                .or_else(|| {
                                    let registry =
                                        world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
                                    let host = registry.host(doc_id)?;
                                    let ast = host.document().ast().result.as_ref().ok().cloned()?;
                                    crate::ast_extract::extract_model_name_from_ast(&ast)
                                })
                                .unwrap_or_default()
                        });
                        if class.is_empty() {
                            bevy::log::info!(
                                "[CanvasDiagram] drop of `{}` ignored — no editable class",
                                def.msl_path
                            );
                        } else {
                            let instance_name = {
                                let state = world.resource::<CanvasDiagramState>();
                                pick_add_instance_name(&def, &state.get(Some(doc_id)).canvas.scene)
                            };
                            // 1. Optimistic synth — node appears immediately.
                            {
                                let mut state =
                                    world.resource_mut::<CanvasDiagramState>();
                                let docstate = state.get_mut(Some(doc_id));
                                synthesize_msl_node(
                                    &mut docstate.canvas.scene,
                                    &def,
                                    &instance_name,
                                    click_world,
                                );
                            }
                            // 2. Source rewrite + canvas_acked_gen bump
                            //    (suppresses the redundant reproject).
                            let op = op_add_component_with_name(
                                &def,
                                &instance_name,
                                click_world,
                                &class,
                            );
                            apply_ops_public(world, doc_id, vec![op]);
                            ui.ctx().request_repaint();
                        }
                    } else {
                        bevy::log::info!(
                            "[CanvasDiagram] drop of `{}` ignored — tab is read-only",
                            def.msl_path
                        );
                    }
                }
                // Always clear the payload on release, hit or miss.
                if let Some(mut payload) = world
                    .get_resource_mut::<crate::ui::panels::palette::ComponentDragPayload>()
                {
                    payload.def = None;
                }
            }
        }

        // Drain in-canvas input control writes from the per-frame
        // queue and apply them to the matching `ModelicaModel.inputs`.
        // The worker forwards the change to `SimStepper::set_input`
        // on the next sync — same path the Telemetry slider uses,
        // just sourced from the diagram's overlay widgets.
        {
            let writes =
                lunco_viz::kinds::canvas_plot_node::drain_input_writes(ui.ctx());
            if !writes.is_empty() {
                // Apply only to THIS canvas's sim. The previous form
                // walked every `ModelicaModel` and matched by input
                // name — a slider on tab A's canvas would silently
                // overwrite the same-named input on tab B's compiled
                // model.
                if let Some(entity) = canvas_sim {
                    if let Some(mut model) = world.get_mut::<crate::ModelicaModel>(entity) {
                        for (name, value) in &writes {
                            if let Some(slot) = model.inputs.get_mut(name) {
                                *slot = *value;
                            }
                        }
                    }
                }
            }
        }

        // Service a deferred Fit request now that the widget rect
        // (`response.rect`) is known. The observer side just sets
        // the flag so the math runs against the real screen size.
        {
            let mut state = world.resource_mut::<CanvasDiagramState>();
            let docstate = state.get_mut(active_doc);
            if docstate.pending_fit {
                docstate.pending_fit = false;
                if let Some(bounds) = docstate.canvas.scene.bounds() {
                    let sr = lunco_canvas::Rect::from_min_max(
                        lunco_canvas::Pos::new(response.rect.min.x, response.rect.min.y),
                        lunco_canvas::Pos::new(response.rect.max.x, response.rect.max.y),
                    );
                    let (c, z) = docstate.canvas.viewport.fit_values(bounds, sr, 40.0);
                    docstate.canvas.viewport.set_target(c, z);
                }
            }
        }

        // Overlay state machine, in priority order:
        //   1. Drill-in load in flight → "Loading <class>…" card.
        //      Highest priority because the document is a placeholder
        //      and anything else (empty summary, etc.) would
        //      misrepresent what's going on.
        //   2. Projection task in flight → "Projecting…" spinner.
        //   3. Empty scene, no task → equation-only model summary.
        let (loading_info, projecting, show_empty_overlay, scene_has_content) = {
            let state = world.resource::<CanvasDiagramState>();
            let loads = world.resource::<DrillInLoads>();
            let dup_loads = world.resource::<DuplicateLoads>();
            let docstate = state.get(active_doc);
            // Unify drill-in + duplicate into a single loading
            // overlay — both are "document is being built off-thread,
            // canvas will populate when the bg task lands."
            let info = active_doc.and_then(|d| {
                loads
                    .progress(d)
                    .or_else(|| dup_loads.progress(d))
                    .map(|(q, secs)| (q.to_string(), secs))
            });
            let has_content = docstate.canvas.scene.node_count() > 0;
            // Diagnostic: log the overlay state once per (doc, state)
            // combination so we can tell whether the loading branch is
            // ever entered. Throw-away — gated behind LUNCO_OVERLAY_TRACE
            // so it doesn't ship in default runs.
            if std::env::var_os("LUNCO_OVERLAY_TRACE").is_some() {
                use std::sync::{Mutex, OnceLock};
                static SEEN: OnceLock<Mutex<std::collections::HashMap<u64, (bool, bool, bool, bool)>>> = OnceLock::new();
                let seen = SEEN.get_or_init(|| Mutex::new(Default::default()));
                let key = active_doc.map(|d| d.raw()).unwrap_or(0);
                let snap = (
                    info.is_some(),
                    has_content,
                    docstate.projection_task.is_some(),
                    loads.is_loading(active_doc.unwrap_or(lunco_doc::DocumentId::new(0))) || dup_loads.is_loading(active_doc.unwrap_or(lunco_doc::DocumentId::new(0))),
                );
                if let Ok(mut m) = seen.lock() {
                    if m.get(&key) != Some(&snap) {
                        m.insert(key, snap);
                        bevy::log::info!(
                            "[Overlay] doc={} info={} has_content={} projecting={} pending_load={}",
                            key, snap.0, snap.1, snap.2, snap.3,
                        );
                    }
                }
            }
            (
                info,
                docstate.projection_task.is_some(),
                !has_content && docstate.projection_task.is_none(),
                has_content,
            )
        };
        // Loading overlay: only on tabs that are genuinely waiting
        // on a drill-in parse (and the scene hasn't been populated
        // yet). Once the scene has content, any brief re-projection
        // from an edit swaps atomically without flashing.
        let theme_snapshot_for_overlay = world
            .get_resource::<lunco_theme::Theme>()
            .cloned()
            .unwrap_or_else(lunco_theme::Theme::dark);
        if let Some((class, secs)) = loading_info {
            if !scene_has_content {
                render_drill_in_loading_overlay(ui, response.rect, &class, secs, &theme_snapshot_for_overlay);
            }
        } else if projecting && !scene_has_content {
            render_projecting_overlay(ui, response.rect, &theme_snapshot_for_overlay);
        } else if show_empty_overlay {
            render_empty_diagram_overlay(ui, response.rect, world);
        }

        // Capture the right-click world position the frame the menu
        // opens — before egui's `press_origin` gets overwritten by
        // later clicks (on menu entries themselves, which would
        // otherwise become the hit-test origin and make a click on
        // empty space appear to have hit a node, or place a newly
        // added component under the menu instead of under the click).
        //
        // The cached value lives on `CanvasDiagramState.context_menu`
        // and is consumed when the menu closes.
        let screen_rect = lunco_canvas::Rect::from_min_max(
            lunco_canvas::Pos::new(response.rect.min.x, response.rect.min.y),
            lunco_canvas::Pos::new(response.rect.max.x, response.rect.max.y),
        );
        // Read whether egui's popup is currently open BEFORE any of
        // our logic runs. This is our ground truth for "is a menu
        // showing right now" — more reliable than our own cache
        // sync, because `context_menu` may open/close between frames
        // without going through our code path.
        let popup_was_open_before = ui.ctx().memory(|m| m.any_popup_open());

        // Track whether this frame wants to dismiss (second-right-
        // click to close). If so, we SKIP `response.context_menu()`
        // entirely for this frame so egui doesn't re-open on the
        // same secondary_clicked signal.
        let mut suppress_menu = tab_read_only;

        if tab_read_only {
            // Stale-menu cleanup when switching to a read-only tab —
            // only fire if WE actually have a cached context menu for
            // this tab. `popup_was_open_before` alone is too broad:
            // egui's any_popup_open() flag also covers the workbench
            // Settings/Help dropdowns, and `close_all_popups` would
            // dismiss those every frame, making them un-clickable
            // whenever a read-only canvas tab is in front.
            let our_menu_cached = world
                .resource::<CanvasDiagramState>()
                .get(active_doc)
                .context_menu
                .is_some();
            if our_menu_cached {
                ui.ctx().memory_mut(|m| m.close_all_popups());
                world
                    .resource_mut::<CanvasDiagramState>()
                    .get_mut(active_doc)
                    .context_menu = None;
            }
        }

        if !tab_read_only && response.secondary_clicked() {
            let press = ui.ctx().input(|i| i.pointer.press_origin());
            if let Some(p) = press.or_else(|| response.interact_pointer_pos()) {
                // Only treat as "dismiss" if this tab itself has a
                // cached menu open. egui's global popup memory can
                // carry a stale popup from a tab we just switched
                // away from (readonly → editable); without this
                // check the first right-click on the new tab gets
                // eaten as a dismiss and the user has to click
                // twice.
                let our_menu_open = popup_was_open_before
                    && world
                        .resource::<CanvasDiagramState>()
                        .get(active_doc)
                        .context_menu
                        .is_some();
                if our_menu_open {
                    // Second right-click while the menu is up → dismiss.
                    // We BOTH clear our cache AND ask egui to close
                    // any popup. Skipping `context_menu` below prevents
                    // egui from re-opening on this same frame.
                    world
                        .resource_mut::<CanvasDiagramState>()
                        .get_mut(active_doc)
                        .context_menu = None;
                    ui.ctx().memory_mut(|m| m.close_all_popups());
                    suppress_menu = true;
                } else {
                    // If egui still thinks a popup is open (from a
                    // previous tab), close it so this frame's
                    // `response.context_menu()` can open our fresh
                    // one without egui deduping against the stale
                    // popup id.
                    if popup_was_open_before {
                        ui.ctx().memory_mut(|m| m.close_all_popups());
                    }
                    // Fresh right-click: capture world position +
                    // hit-test origin while `press_origin` still
                    // reflects the right-click (before any menu-entry
                    // click overwrites it).
                    let state = world.resource::<CanvasDiagramState>();
                    let docstate = state.get(active_doc);
                    let world_pos = docstate.canvas.viewport.screen_to_world(
                        lunco_canvas::Pos::new(p.x, p.y),
                        screen_rect,
                    );
                    let hit_node = docstate.canvas.scene.hit_node(world_pos, 6.0);
                    let hit_edge = docstate.canvas.scene.hit_edge(world_pos, 4.0);
                    let target = match (hit_node, hit_edge) {
                        (Some((id, _)), _) => ContextMenuTarget::Node(id),
                        (_, Some(id)) => ContextMenuTarget::Edge(id),
                        _ => ContextMenuTarget::Empty,
                    };
                    let _ = state;
                    bevy::log::info!(
                        "[CanvasDiagram] right-click screen=({:.1},{:.1}) world=({:.1},{:.1}) target={:?}",
                        p.x, p.y, world_pos.x, world_pos.y, target
                    );
                    world
                        .resource_mut::<CanvasDiagramState>()
                        .get_mut(active_doc)
                        .context_menu = Some(PendingContextMenu {
                        screen_pos: p,
                        world_pos,
                        target,
                    });
                }
            }
        }

        // ── Render menu via egui's native `context_menu`. ──
        // Content comes from the cached PendingContextMenu (above).
        // Skipped on the dismiss-frame so egui doesn't re-open.
        let menu_ops: Vec<ModelicaOp> = if suppress_menu {
            Vec::new()
        } else {
            let mut collected: Vec<ModelicaOp> = Vec::new();
            let cached = world
                .resource::<CanvasDiagramState>()
                .get(active_doc)
                .context_menu
                .clone();
            response.context_menu(|ui| {
                let Some(menu) = cached.as_ref() else {
                    // No cached data — shouldn't happen since
                    // context_menu only opens after secondary_clicked,
                    // but render a minimal placeholder just in case.
                    ui.label("(no click target)");
                    return;
                };
                match &menu.target {
                    ContextMenuTarget::Node(id) => {
                        render_node_menu(
                            ui,
                            world,
                            *id,
                            editing_class.as_deref(),
                            &mut collected,
                        );
                    }
                    ContextMenuTarget::Edge(id) => {
                        render_edge_menu(
                            ui,
                            world,
                            *id,
                            editing_class.as_deref(),
                            &mut collected,
                        );
                    }
                    ContextMenuTarget::Empty => {
                        render_empty_menu(
                            ui,
                            world,
                            menu.world_pos,
                            editing_class.as_deref(),
                            &mut collected,
                        );
                    }
                }
            });
            collected
        };

        // Sync our cache with egui's popup state, AFTER context_menu
        // has had a chance to open/close this frame. If egui closed
        // the popup (user clicked outside, pressed escape, picked
        // an entry) and we still have a cache, drop the cache.
        // Running this *after* keeps us from clearing the cache we
        // just populated on a fresh right-click.
        let popup_open_now = ui.ctx().memory(|m| m.any_popup_open());
        if !popup_open_now
            && world
                .resource::<CanvasDiagramState>()
                .get(active_doc)
                .context_menu
                .is_some()
        {
            world
                .resource_mut::<CanvasDiagramState>()
                .get_mut(active_doc)
                .context_menu = None;
        }

        // Double-click on a node → "drill in". Open the class that
        // the component instantiates as a new model view tab,
        // alongside the current one. Matches Dymola / OMEdit's
        // "go into this component" gesture.
        for ev in &events {
            if let lunco_canvas::SceneEvent::NodeDoubleClicked { id } = ev {
                let type_name = {
                    let state = world.resource::<CanvasDiagramState>();
                    state
                        .get(active_doc)
                        .canvas
                        .scene
                        .node(*id)
                        .and_then(|n| n.data.downcast_ref::<IconNodeData>())
                        .map(|d| d.qualified_type.clone())
                };
                if let Some(qualified) = type_name {
                    drill_into_class(world, &qualified);
                }
            }
        }

        // Translate scene events → ModelicaOps and apply via the
        // Reflect command surface (per AGENTS.md §4.1 rule 3). The
        // helper converts to the API mirror enum and fires
        // `ApplyModelicaOps`; the observer hands them to
        // `apply_ops_public`, which is the same code path that
        // already serviced this site directly.
        if let (Some(doc_id), Some(class)) = (doc_id, editing_class.as_ref()) {
            let mut all_ops = build_ops_from_events(world, &events, class);
            all_ops.extend(menu_ops);
            if !all_ops.is_empty() {
                #[cfg(feature = "lunco-api")]
                crate::api_edits::trigger_apply_ops(world, doc_id, all_ops);
                #[cfg(not(feature = "lunco-api"))]
                apply_ops(world, doc_id, all_ops);
            }
        } else if !menu_ops.is_empty() {
            bevy::log::warn!(
                "[CanvasDiagram] menu emitted {} op(s) but no editing class — discarded",
                menu_ops.len()
            );
        }
        // `events` is consumed by `build_ops_from_events`; suppress
        // the unused warning when `doc_id`/`class` were absent.
        let _ = events;

        mark("tail (events/menu/fit)", &mut phase_t, &mut phase_log);
        if trace_phases && !phase_log.is_empty() {
            let total: f64 = phase_log.iter().map(|(_, ms)| *ms).sum();
            if total > 30.0 {
                let breakdown = phase_log
                    .iter()
                    .map(|(name, ms)| format!("{name}={ms:.1}ms"))
                    .collect::<Vec<_>>()
                    .join(" ");
                bevy::log::info!(
                    "[CanvasDiagram] render_canvas phases (sum={total:.1}ms): {breakdown}"
                );
            }
        }
    }
}

// ─── MSL package tree (for nested add-component menu) ──────────────

/// One node in the MSL package hierarchy. `classes` are instantiable
/// at this level (instances we'd add to the diagram), `subpackages`
/// are deeper navigation. `BTreeMap` for stable alphabetical order
/// regardless of the source list's order.
struct MslPackageNode {
    subpackages: std::collections::BTreeMap<String, MslPackageNode>,
    /// Classes at this level. Pre-sorted alphabetically by short name
    /// once at tree-build time so `render_msl_package_menu` doesn't
    /// clone-and-sort on every render frame (the menu re-renders
    /// every frame the pointer is over it; per-frame O(n log n)
    /// across nested submenus is the cause of the laggy right-click
    /// context-menu navigation).
    classes: Vec<&'static MSLComponentDef>,
    /// Pre-computed: `true` if this subtree contains at least one
    /// non-icon-only class. Lets the menu skip empty branches in O(1)
    /// instead of recursively walking on every render.
    has_non_icon_class: bool,
}

impl MslPackageNode {
    fn new() -> Self {
        Self {
            subpackages: Default::default(),
            classes: Vec::new(),
            has_non_icon_class: false,
        }
    }
}

/// User-facing toggles for the MSL add-component menu. Default
/// values are tuned for the common case ("a user dropping a
/// component expects a functional block, not an icon shell").
/// Persisted as a Bevy resource; the Settings dropdown flips the
/// `show_icon_only_classes` flag to override.
#[derive(Resource, Debug, Clone)]
pub struct PaletteSettings {
    /// When `true`, pure-icon classes (matched by
    /// [`crate::ui::loaded_classes::is_icon_only_class`]) appear in the
    /// MSL add-component submenus. Default `false` — matches
    /// Dymola's "hide `.Icons.*`" default.
    pub show_icon_only_classes: bool,
}

impl Default for PaletteSettings {
    fn default() -> Self {
        Self {
            show_icon_only_classes: false,
        }
    }
}

/// Soft guards for the canvas projection. Prevent accidental
/// attempts to diagram huge packages without getting in the way of
/// deeply composed real models. Exposed via the Settings dropdown.
#[derive(Resource, Debug, Clone)]
pub struct DiagramProjectionLimits {
    /// Maximum component count the projector will accept before
    /// returning `None`. Default
    /// [`crate::ui::panels::canvas_projection::DEFAULT_MAX_DIAGRAM_NODES`]
    /// (1000). Users building power-system or multi-body models
    /// with hundreds of components can raise this in Settings.
    pub max_nodes: usize,
    /// Wall-clock deadline for a single projection task. If the bg
    /// task hasn't resolved within this window, the poll loop
    /// flips the task's `cancel` flag AND drops the handle. Task
    /// finishes (waste, but bounded), result is discarded, canvas
    /// stays empty with a "projection timed out" overlay.
    ///
    /// Deliberately high (60 s default) — only catches truly
    /// catastrophic work, not normal drill-ins. Raise in Settings
    /// if you're profiling something slow on purpose.
    pub max_duration: std::time::Duration,
}

impl Default for DiagramProjectionLimits {
    fn default() -> Self {
        Self {
            max_nodes: crate::ui::panels::canvas_projection::DEFAULT_MAX_DIAGRAM_NODES,
            max_duration: std::time::Duration::from_secs(60),
        }
    }
}

/// True if the subtree contains any class that would be visible
/// with the icon-only filter OFF (i.e. has a real, non-icon-only
/// class somewhere). Reads the precomputed flag set at tree-build
/// time so the menu can skip empty branches in O(1).
///
/// Was previously recursive — fine for one open-frame, expensive
/// when called on every render for every visible submenu (the
/// right-click menu re-runs every frame the pointer is over it).
fn package_has_visible_classes(node: &MslPackageNode) -> bool {
    node.has_non_icon_class
}

/// Lazily-built package tree. Walks every entry in
/// [`crate::visual_diagram::msl_component_library`] once and
/// inserts it under its dotted package path. Cached for the life
/// of the process — MSL content doesn't change at runtime.
fn msl_package_tree() -> &'static MslPackageNode {
    use std::sync::OnceLock;
    static TREE: OnceLock<MslPackageNode> = OnceLock::new();
    TREE.get_or_init(|| {
        let mut root = MslPackageNode::new();
        for comp in crate::visual_diagram::msl_component_library() {
            // Split the qualified path into package segments + a
            // trailing class name. `Modelica.Electrical.Analog.
            // Basic.Resistor` → walk subpackages
            // [Modelica, Electrical, Analog, Basic], attach class
            // `Resistor`.
            let mut parts: Vec<&str> = comp.msl_path.split('.').collect();
            let Some(_class_name) = parts.pop() else { continue };
            let mut node = &mut root;
            for seg in parts {
                node = node
                    .subpackages
                    .entry(seg.to_string())
                    .or_insert_with(MslPackageNode::new);
            }
            node.classes.push(comp);
        }
        // Post-pass: sort classes alphabetically by short name and
        // precompute the `has_non_icon_class` rollup. Done once here
        // so the right-click menu's recursive renderer is purely
        // O(visible-items) per frame instead of repeatedly cloning,
        // sorting, and walking subtrees.
        finalize_tree(&mut root);
        root
    })
}

fn finalize_tree(node: &mut MslPackageNode) {
    node.classes.sort_by(|a, b| a.name.cmp(&b.name));
    let mut any_visible = node
        .classes
        .iter()
        .any(|c| !crate::ui::loaded_classes::is_icon_only_class(&c.msl_path));
    for child in node.subpackages.values_mut() {
        finalize_tree(child);
        any_visible = any_visible || child.has_non_icon_class;
    }
    node.has_non_icon_class = any_visible;
}

/// Recursively render a package node as egui submenus.
///
/// Ordering per level: subpackages first (alphabetical via
/// `BTreeMap`), then a thin separator, then classes at this
/// level (own-package classes). Matches how OMEdit's library
/// browser reads: packages above, classes below.
///
/// On click of a class item we emit `AddComponent` through `out`
/// exactly as the flat menu did.
fn render_msl_package_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    doc_id: Option<lunco_doc::DocumentId>,
    node: &MslPackageNode,
    click_world: lunco_canvas::Pos,
    editing_class: Option<&str>,
    show_icons: bool,
    out: &mut Vec<ModelicaOp>,
) {
    for (name, child) in &node.subpackages {
        // Skip subtrees that would be entirely empty after the
        // icon-only filter. Cheap recursive walk; avoids showing
        // dead-end submenus the user can click into only to find
        // nothing.
        if !show_icons && !package_has_visible_classes(child) {
            continue;
        }
        ui.menu_button(name, |ui| {
            render_msl_package_menu(
                ui, world, doc_id, child, click_world, editing_class, show_icons, out,
            );
        });
    }
    if !node.subpackages.is_empty() && !node.classes.is_empty() {
        ui.separator();
    }
    // Classes are pre-sorted at tree-build time (see `finalize_tree`).
    // Iterating directly avoids a clone + sort on every render frame.
    for comp in &node.classes {
        let comp = *comp;
        // Hide icon-only classes unless the user explicitly enabled
        // them in Settings. Path-based detection via `is_icon_only_class`
        // (currently `.Icons.` subpackage check).
        if !show_icons && crate::ui::loaded_classes::is_icon_only_class(&comp.msl_path) {
            continue;
        }
        // Display: icon character (if any) + short name. The
        // icon character gives a quick visual cue without
        // loading the SVG.
        let label = if let Some(ic) = comp.icon_text.as_deref() {
            if !ic.is_empty() {
                format!("{ic}  {}", comp.name)
            } else {
                comp.name.clone()
            }
        } else {
            comp.name.clone()
        };
        if ui
            .button(label)
            .on_hover_text(
                comp.description
                    .clone()
                    .unwrap_or_else(|| comp.msl_path.clone()),
            )
            .clicked()
        {
            if let Some(class) = editing_class {
                let instance_name = {
                    let state = world.resource::<CanvasDiagramState>();
                    pick_add_instance_name(comp, &state.get(doc_id).canvas.scene)
                };
                // Optimistic synthesis: the canvas reflects the new
                // node *before* the AST settles, so the user sees an
                // instant response. `apply_ops` then bumps
                // `canvas_acked_gen` to suppress the redundant
                // reproject when the AST does land.
                {
                    let mut state = world.resource_mut::<CanvasDiagramState>();
                    let docstate = state.get_mut(doc_id);
                    synthesize_msl_node(
                        &mut docstate.canvas.scene,
                        comp,
                        &instance_name,
                        click_world,
                    );
                }
                out.push(op_add_component_with_name(comp, &instance_name, click_world, class));
            }
            ui.close();
        }
    }
}

// ─── Context-menu renderers ────────────────────────────────────────

fn render_node_menu(
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
        let state = world.resource::<CanvasDiagramState>();
        state
            .get(active_doc)
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
                let mut state = world.resource_mut::<CanvasDiagramState>();
                let docstate = state.get_mut(active_doc);
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

fn render_plot_node_menu(
    ui: &mut egui::Ui,
    world: &mut World,
    id: lunco_canvas::NodeId,
) {
    use lunco_viz::kinds::canvas_plot_node::PlotNodeData;

    let current: PlotNodeData = {
        let active_doc = active_doc_from_world(world);
        let state = world.resource::<CanvasDiagramState>();
        state
            .get(active_doc)
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
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = state.get_mut(active_doc);
        docstate.canvas.scene.remove_node(id);
        ui.close();
    }
}

fn rebind_plot_node(
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
    let mut state = world.resource_mut::<CanvasDiagramState>();
    let docstate = state.get_mut(active_doc);
    if let Some(node) = docstate.canvas.scene.node_mut(id) {
        node.data = data;
    }
}

fn render_edge_menu(
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
                let mut state = world.resource_mut::<CanvasDiagramState>();
                let docstate = state.get_mut(active_doc);
                docstate.canvas.scene.remove_edge(id);
            }
        }
        ui.close();
    }
    if ui.button("↺ Reverse direction").clicked() {
        ui.close();
    }
}

fn render_empty_menu(
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
    render_msl_package_menu(
        ui,
        world,
        active_doc,
        msl_package_tree(),
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
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = state.get_mut(active_doc);
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
        let mut state = world.resource_mut::<CanvasDiagramState>();
        let docstate = state.get_mut(active_doc);
        let c = docstate.canvas.viewport.center;
        docstate.canvas.viewport.set_target(c, 1.0);
        ui.close();
    }
}

/// Shorthand used by free helpers that don't already have the
/// active doc threaded through: resolve it from the Workspace session.
/// Kept inline so callers outside the main render flow don't grow a
/// parameter just to pass a one-line lookup.
fn active_doc_from_world(world: &World) -> Option<lunco_doc::DocumentId> {
    world
        .resource::<lunco_workbench::WorkspaceResource>()
        .active_document
}

/// Insert a plot scene node anchored at `click_world`. `entity_bits = 0`
/// + empty `signal_path` is the unbound form — the visual draws an
/// empty card the user can resize and bind later from the inspector.
fn insert_plot_node(
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
    let mut state = world.resource_mut::<CanvasDiagramState>();
    let docstate = state.get_mut(active_doc);
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

// ─── Drill-in loading overlay ──────────────────────────────────────

/// Rendered while a background file-read (and subsequent
/// `ReplaceSource` re-parse) is running for a drill-in target.
/// Named class, animated dots — same visual language as the
/// projection overlay but a different message so the user knows
/// it's a fresh load, not a re-project.
fn render_drill_in_loading_overlay(
    ui: &mut egui::Ui,
    canvas_rect: egui::Rect,
    class_name: &str,
    elapsed_secs: f32,
    theme: &lunco_theme::Theme,
) {
    let card_w = 340.0;
    let card_h = 84.0;
    let card_rect = egui::Rect::from_center_size(
        canvas_rect.center(),
        egui::vec2(card_w, card_h),
    );
    let painter = ui.painter();
    let shadow = {
        let b = theme.colors.base;
        egui::Color32::from_rgba_unmultiplied(b.r(), b.g(), b.b(), 100)
    };
    painter.rect_filled(
        card_rect.translate(egui::vec2(0.0, 3.0)),
        8.0,
        shadow,
    );
    painter.rect_filled(card_rect, 8.0, theme.tokens.surface_raised);
    painter.rect_stroke(
        card_rect,
        8.0,
        egui::Stroke::new(1.0, theme.tokens.surface_raised_border),
        egui::StrokeKind::Outside,
    );
    let t = ui.ctx().input(|i| i.time) as f32;
    let spinner_center = egui::pos2(card_rect.min.x + 28.0, card_rect.center().y);
    let accent = theme.tokens.accent;
    for i in 0..3 {
        let phase = (t * 2.5 - i as f32 * 0.4).rem_euclid(std::f32::consts::TAU);
        let alpha = ((phase.sin() * 0.5 + 0.5) * 255.0) as u8;
        let col = egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
        painter.circle_filled(
            spinner_center + egui::vec2(i as f32 * 9.0 - 9.0, 0.0),
            3.5,
            col,
        );
    }
    // Header line: "Loading resource… 12s" — the elapsed counter
    // reassures the user during slow rumoca parses (large package
    // files can take tens of seconds). Hidden in the first 0.5s to
    // avoid flicker on fast loads.
    let header = if elapsed_secs < 0.5 {
        "Loading resource…".to_string()
    } else if elapsed_secs < 10.0 {
        format!("Loading resource… {:.1}s", elapsed_secs)
    } else {
        format!("Loading resource… {}s", elapsed_secs.round() as u32)
    };
    painter.text(
        egui::pos2(card_rect.min.x + 60.0, card_rect.center().y - 8.0),
        egui::Align2::LEFT_CENTER,
        header,
        egui::FontId::proportional(13.0),
        theme.tokens.text,
    );
    // Trim long qualified names with ellipsis on the left so the
    // short class name stays visible.
    let display = if class_name.len() > 40 {
        format!("…{}", &class_name[class_name.len() - 39..])
    } else {
        class_name.to_string()
    };
    painter.text(
        egui::pos2(card_rect.min.x + 60.0, card_rect.center().y + 10.0),
        egui::Align2::LEFT_CENTER,
        display,
        egui::FontId::monospace(11.0),
        theme.tokens.text_subdued,
    );
    // Animating — request repaint so the spinner moves smoothly.
    ui.ctx().request_repaint();
}

// ─── Loading / projection overlay ──────────────────────────────────

/// Small "Projecting…" card centred on the canvas while an
/// `AsyncComputeTaskPool` projection task is in flight. Includes
/// a rotating dot so users can see the UI is responsive.
fn render_projecting_overlay(ui: &mut egui::Ui, canvas_rect: egui::Rect, theme: &lunco_theme::Theme) {
    let card_w = 260.0;
    let card_h = 72.0;
    let card_rect = egui::Rect::from_center_size(
        canvas_rect.center(),
        egui::vec2(card_w, card_h),
    );
    let painter = ui.painter();
    let shadow = {
        let b = theme.colors.base;
        egui::Color32::from_rgba_unmultiplied(b.r(), b.g(), b.b(), 90)
    };
    painter.rect_filled(
        card_rect.translate(egui::vec2(0.0, 3.0)),
        8.0,
        shadow,
    );
    painter.rect_filled(card_rect, 8.0, theme.tokens.surface_raised);
    painter.rect_stroke(
        card_rect,
        8.0,
        egui::Stroke::new(1.0, theme.tokens.surface_raised_border),
        egui::StrokeKind::Outside,
    );

    // Animated spinner — three dots pulsing in sequence via
    // `ctx.input(|i| i.time)`. Frame-rate independent.
    let t = ui.ctx().input(|i| i.time) as f32;
    let spinner_center = egui::pos2(card_rect.min.x + 28.0, card_rect.center().y);
    let accent = theme.tokens.accent;
    for i in 0..3 {
        let phase = (t * 2.5 - i as f32 * 0.4).rem_euclid(std::f32::consts::TAU);
        let alpha = ((phase.sin() * 0.5 + 0.5) * 255.0) as u8;
        let col = egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
        painter.circle_filled(
            spinner_center + egui::vec2(i as f32 * 9.0 - 9.0, 0.0),
            3.0,
            col,
        );
    }
    painter.text(
        egui::pos2(card_rect.min.x + 60.0, card_rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Loading resource…",
        egui::FontId::proportional(13.0),
        theme.tokens.text,
    );
}

// ─── Empty-diagram summary ──────────────────────────────────────────

/// When the canvas scene has no nodes — common for equation-only
/// leaf models (Battery, RocketEngine, BouncyBall, SpringMass) and
/// MSL building blocks (Integrator, Resistor, Inertia) — paint a
/// "data sheet" card in the centre of the canvas. Treats the class
/// as a first-class display object instead of leaving the user
/// staring at the blank grid.
///
/// Card layout:
/// 1. **Hero strip** — the class's authored `Icon(graphics={...})`
///    annotation rendered via [`crate::icon_paint::paint_graphics`].
///    For classes without one, a stylised type-badge (M / B / C / …).
/// 2. **Heading** — class name + type label.
/// 3. **Symbol bands** — named parameters / inputs / outputs (top 6
///    each). Names beat counts: "tau, J, c" tells the user what the
///    model is for; "3 parameters" doesn't.
/// 4. **Footer counts** — equations + connect equations as a one-
///    line summary, plus a hint that points at the Text tab.
fn render_empty_diagram_overlay(
    ui: &mut egui::Ui,
    canvas_rect: egui::Rect,
    world: &mut World,
) {
    let Some(open) = world.resource::<WorkbenchState>().open_model.clone() else {
        return;
    };
    let theme = world
        .get_resource::<lunco_theme::Theme>()
        .cloned()
        .unwrap_or_else(lunco_theme::Theme::dark);
    let source = open.source.clone();
    let class_name = open
        .detected_name
        .clone()
        .unwrap_or_else(|| "(unnamed)".into());

    let counts = empty_overlay_counts_cached(source.as_ref());

    // Pull the live class info out of the document registry so we
    // can show real symbol names + (when authored) the class's own
    // `Icon` graphics. This is the same AST the canvas projector
    // already holds, so we don't pay a re-parse.
    let active_doc = active_doc_from_world(world);
    let (icon, class_type, description, param_names, input_names, output_names) =
        empty_overlay_class_info(world, active_doc, &class_name);

    crate::ui::panels::placeholder::render_centered_card(
        ui,
        canvas_rect,
        egui::vec2(440.0, 360.0),
        &theme,
        |child| {
            // ── Hero strip ────────────────────────────────────────
            // Either the authored icon or a stylised type badge.
            let hero_size = egui::vec2(120.0, 80.0);
            let (_, hero_rect) = child.allocate_space(hero_size);
            if let Some(icon) = &icon {
                crate::icon_paint::paint_graphics(
                    child.painter(),
                    hero_rect,
                    icon.coordinate_system,
                    &icon.graphics,
                );
            } else {
                paint_class_type_badge(
                    child.painter(),
                    hero_rect,
                    class_type.unwrap_or("model"),
                    &theme,
                );
            }
            child.add_space(8.0);

            // ── Class name + type label ───────────────────────────
            child.label(
                egui::RichText::new(&class_name)
                    .strong()
                    .size(15.0)
                    .color(theme.text_heading()),
            );
            if let Some(t) = class_type {
                child.label(
                    egui::RichText::new(t)
                        .size(10.5)
                        .italics()
                        .color(theme.text_muted()),
                );
            }
            if let Some(desc) = &description {
                child.add_space(4.0);
                child.label(
                    egui::RichText::new(desc)
                        .size(11.0)
                        .color(theme.tokens.text),
                );
            }
            child.add_space(8.0);
            child.separator();
            child.add_space(6.0);

            // ── Named symbol bands ───────────────────────────────
            paint_symbol_band(child, "Parameters", &param_names, counts.params, &theme);
            paint_symbol_band(child, "Inputs", &input_names, counts.inputs, &theme);
            paint_symbol_band(child, "Outputs", &output_names, counts.outputs, &theme);

            child.add_space(6.0);
            child.label(
                egui::RichText::new(format!(
                    "{} equations · {} connect equations",
                    counts.equations, counts.connects,
                ))
                .small()
                .color(theme.text_muted()),
            );
            child.add_space(4.0);
            child.label(
                egui::RichText::new("→ Switch to the Text tab to read / edit the source.")
                    .italics()
                    .size(10.0)
                    .color(theme.text_muted()),
            );
        },
    );
}

/// Pull human-friendly info about the active class: authored Icon,
/// type keyword (`model`/`block`/…), description string, and the top
/// few parameter / input / output names. Falls back to `None`/empty
/// vectors silently when the registry doesn't have the doc.
fn empty_overlay_class_info(
    world: &mut World,
    doc_id: Option<lunco_doc::DocumentId>,
    class_name: &str,
) -> (
    Option<crate::annotations::Icon>,
    Option<&'static str>,
    Option<String>,
    Vec<String>,
    Vec<String>,
    Vec<String>,
) {
    let Some(doc) = doc_id else {
        return (None, None, None, vec![], vec![], vec![]);
    };
    let registry = world.resource::<ModelicaDocumentRegistry>();
    let Some(host) = registry.host(doc) else {
        return (None, None, None, vec![], vec![], vec![]);
    };
    let document = host.document();
    let ast_arc = match document.ast().result.as_ref() {
        Ok(a) => a.clone(),
        Err(_) => return (None, None, None, vec![], vec![], vec![]),
    };

    // Locate the class. Prefer an exact name match; fall back to the
    // first non-package class (matches `extract_model_name`).
    let class_def = locate_class(&ast_arc, class_name);
    let Some(class) = class_def else {
        return (None, None, None, vec![], vec![], vec![]);
    };

    use rumoca_session::parsing::ast::Causality;
    use rumoca_session::parsing::ClassType;

    // Walk the `extends` chain so classes that inherit their Icon
    // (e.g. `Modelica.Fluid.Valves.ValveCompressible` → `PartialValve`)
    // still render with the parent's glyph. Mirrors the resolver
    // pattern in `canvas_projection::register_local_class` —
    // local-AST first, then non-blocking MSL cache peek.
    let icon = {
        use std::sync::Arc;
        let ast_for_resolver = ast_arc.clone();
        let mut resolver =
            |name: &str| -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
                let leaf = name.rsplit('.').next().unwrap_or(name);
                if let Some(c) = ast_for_resolver
                    .classes
                    .get(name)
                    .or_else(|| ast_for_resolver.classes.get(leaf))
                    .or_else(|| {
                        ast_for_resolver
                            .classes
                            .values()
                            .flat_map(|c| c.classes.values())
                            .find(|c| c.name.text.as_ref() == leaf)
                    })
                {
                    return Some(Arc::new(c.clone()));
                }
                crate::class_cache::peek_msl_class_cached(name)
            };
        let mut visited = std::collections::HashSet::new();
        let class_context = match ast_arc.within.as_ref() {
            Some(within) => {
                let pkg = within
                    .name
                    .iter()
                    .map(|t| t.text.as_ref())
                    .collect::<Vec<_>>()
                    .join(".");
                if pkg.is_empty() {
                    class_name.to_string()
                } else {
                    format!("{pkg}.{class_name}")
                }
            }
            None => class_name.to_string(),
        };
        crate::annotations::extract_icon_inherited(
            &class_context,
            class,
            &mut resolver,
            &mut visited,
        )
    };
    let class_type = match class.class_type {
        ClassType::Model => Some("model"),
        ClassType::Block => Some("block"),
        ClassType::Class => Some("class"),
        ClassType::Connector => Some("connector"),
        ClassType::Record => Some("record"),
        ClassType::Type => Some("type"),
        ClassType::Package => Some("package"),
        ClassType::Function => Some("function"),
        ClassType::Operator => Some("operator"),
    };
    let description: Option<String> = class
        .description
        .iter()
        .next()
        .map(|t| t.text.as_ref().trim_matches('"').to_string())
        .filter(|s| !s.is_empty());

    let mut params = Vec::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for (name, comp) in class.components.iter() {
        use rumoca_session::parsing::ast::Variability;
        if matches!(comp.variability, Variability::Parameter(_)) {
            params.push(name.clone());
        }
        match comp.causality {
            Causality::Input(_) => inputs.push(name.clone()),
            Causality::Output(_) => outputs.push(name.clone()),
            _ => {}
        }
    }

    (icon, class_type, description, params, inputs, outputs)
}

/// Extract the `Diagram(graphics={...})` annotation for the target
/// class — full-qualified drill-in target, or the first non-package
/// class when no drill-in is active. Used by the background
/// decoration layer to paint MSL-style diagram callouts (labelled
/// regions, accent text) behind the nodes.
/// Emit canvas Nodes for every interactive item in the active
/// class's `Diagram(graphics=…)`. Today that's:
///   * `__LunCo_PlotNode` → `lunco.viz.plot` (live signal tile)
///   * `Text` → `lunco.modelica.text` (editable label)
///
/// Each emitted Node carries a stable `origin` marker derived from
/// the annotation's position in the source (`plot:<idx>:<signal>` or
/// `text:<idx>`) so the canvas-edit pipeline recognises it as
/// source-backed and the carry-over filter doesn't double-insert.
/// Returns the set of emitted origin keys.
fn emit_diagram_decorations(
    scene: &mut lunco_canvas::scene::Scene,
    graphics: &[crate::annotations::GraphicItem],
) -> std::collections::HashSet<String> {
    use crate::annotations::GraphicItem;
    let mut origins: std::collections::HashSet<String> = Default::default();
    let mut text_idx: usize = 0;
    for (idx, item) in graphics.iter().enumerate() {
        if let GraphicItem::Text(t) = item {
            // Editable label. Strip surrounding quotes the parser
            // left on `textString` so the visual sees the raw
            // string. Skip `%name` / `%class` substitutions and
            // empty strings — those are MSL conventions for
            // icon-internal Text and aren't meaningful as Diagram
            // callouts.
            let raw = t.text_string.trim_matches('"');
            if raw.is_empty() || raw.starts_with('%') {
                text_idx += 1;
                continue;
            }
            let payload = crate::ui::text_node::TextNodeData {
                text: raw.to_string(),
                font_size: t.font_size,
                color: t.text_color.map(|c| [c.r, c.g, c.b]),
            };
            let data: lunco_canvas::NodeData = std::sync::Arc::new(payload);
            let origin = format!("text:{text_idx}");
            origins.insert(origin.clone());
            let id = scene.alloc_node_id();
            // Same Y-flip + corner-normalize the plot path uses.
            // Modelica `extent` is +Y up, canvas world is +Y down.
            let x1 = t.extent.p1.x as f32;
            let x2 = t.extent.p2.x as f32;
            let y1 = -(t.extent.p1.y as f32);
            let y2 = -(t.extent.p2.y as f32);
            let rect = lunco_canvas::Rect::from_min_max(
                lunco_canvas::Pos::new(x1.min(x2), y1.min(y2)),
                lunco_canvas::Pos::new(x1.max(x2), y1.max(y2)),
            );
            scene.insert_node(lunco_canvas::scene::Node {
                id,
                rect,
                kind: crate::ui::text_node::TEXT_NODE_KIND.into(),
                data,
                ports: Vec::new(),
                label: String::new(),
                origin: Some(origin),
                resizable: true,
                visual_rect: None,
            });
            text_idx += 1;
            continue;
        }
        let GraphicItem::LunCoPlotNode(plot) = item else { continue };
        // `entity` is the runtime Bevy id of the simulator host that
        // produces samples for this signal. We don't know it at
        // projection time (the source can be loaded long before the
        // sim spawns), so we leave it as `Entity::PLACEHOLDER` (0)
        // — the live sample resolver in
        // `lunco_viz::canvas_plot_node` keys by signal path and
        // recovers the active producer at fetch time.
        let payload = lunco_viz::kinds::canvas_plot_node::PlotNodeData {
            entity: 0,
            signal_path: plot.signal.clone(),
            title: plot.title.clone(),
        };
        let data: lunco_canvas::NodeData = std::sync::Arc::new(payload);
        let origin = format!("plot:{idx}:{}", plot.signal);
        origins.insert(origin.clone());
        let id = scene.alloc_node_id();
        scene.insert_node(lunco_canvas::scene::Node {
            id,
            rect: {
                // Modelica diagrams are +Y up; canvas world is +Y
                // down (`coords::modelica_to_canvas` negates Y).
                // Apply the flip per corner, then normalise so
                // `from_min_max` gets `min < max` on both axes —
                // Modelica `extent={{x1,y1},{x2,y2}}` doesn't
                // enforce corner ordering. Without this two
                // failures stack: a flipped Y range that puts the
                // tile far above the icons instead of below them,
                // and a zero-area rect when the source's first
                // corner has the larger y.
                let x1 = plot.extent.p1.x as f32;
                let x2 = plot.extent.p2.x as f32;
                let y1 = -(plot.extent.p1.y as f32);
                let y2 = -(plot.extent.p2.y as f32);
                lunco_canvas::Rect::from_min_max(
                    lunco_canvas::Pos::new(x1.min(x2), y1.min(y2)),
                    lunco_canvas::Pos::new(x1.max(x2), y1.max(y2)),
                )
            },
            kind: lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND.into(),
            data,
            ports: Vec::new(),
            label: String::new(),
            origin: Some(origin),
            resizable: true,
            visual_rect: None,
        });
    }
    origins
}

fn diagram_annotation_for_target(
    ast: &rumoca_session::parsing::ast::StoredDefinition,
    target: Option<&str>,
) -> Option<crate::annotations::Diagram> {
    // Resolve the target class by qualified path walk (supports the
    // MSL `Modelica.Blocks.Examples.PID_Controller` style). For `None`
    // targets fall back to the first non-package class, matching the
    // workbench's default active-class picker.
    let class = if let Some(qualified) = target {
        walk_qualified(ast, qualified)
    } else {
        use rumoca_session::parsing::ClassType;
        ast.classes
            .iter()
            .find(|(_, c)| !matches!(c.class_type, ClassType::Package))
            .map(|(_, c)| c)
    };
    class.and_then(|c| crate::annotations::extract_diagram(&c.annotation))
}

/// SI unit suffix for the most common `Modelica.Units.SI.*` types used
/// by MSL Mechanics / Electrical / Blocks. Returned string is appended
/// to `%paramName` substitutions so the canvas matches OMEdit's
/// "value + unit" presentation (`J=2 kg.m2`, `c=1e4 N.m/rad`, …).
///
/// TODO: replace with proper type resolution. The authoritative source
/// is the type's declaration — `type Torque = Real(unit="N.m")` — not a
/// hand-maintained table. Plumb `unit` through `msl_indexer` (resolve
/// `comp.type_name` via scope chain + `class_cache`, walk the
/// `extends Real(unit=...)` modification) so `ParamDef.unit` is
/// populated from source. Once that lands, drop this fn and read
/// `p.unit` directly. Stopgap covers the high-frequency MSL types so
/// the PID example matches OMEdit; user-defined SI types (e.g.
/// `type Pressure = Real(unit="Pa")` in user models) fall through to
/// the bare value until the proper resolver is in.
fn si_unit_suffix(param_type: &str) -> Option<&'static str> {
    let leaf = param_type.rsplit('.').next().unwrap_or(param_type);
    Some(match leaf {
        "Torque" => "N.m",
        "Inertia" => "kg.m2",
        "Mass" => "kg",
        "Length" => "m",
        "Distance" => "m",
        "Time" => "s",
        "Angle" => "rad",
        "AngularVelocity" => "rad/s",
        "AngularAcceleration" => "rad/s2",
        "Velocity" => "m/s",
        "Acceleration" => "m/s2",
        "Force" => "N",
        "Power" => "W",
        "Energy" => "J",
        "Frequency" => "Hz",
        "Temperature" | "ThermodynamicTemperature" => "K",
        "Voltage" => "V",
        "Current" => "A",
        "Resistance" => "Ohm",
        "Capacitance" => "F",
        "Inductance" => "H",
        "RotationalSpringConstant" | "TranslationalSpringConstant" => "N.m/rad",
        "RotationalDampingConstant" | "TranslationalDampingConstant" => "N.m.s/rad",
        _ => return None,
    })
}

/// Walk a dotted qualified class path through `ast.classes` into
/// nested `class.classes`. Returns the deepest matching class, if any.
///
/// Honours the file's `within` clause: MSL files like
/// `Modelica/Blocks/package.mo` start with `within Modelica;`, so their
/// AST root contains `Blocks`, not `Modelica`. A drill-in target of
/// `Modelica.Blocks.Examples.PID_Controller` must therefore have the
/// `Modelica` prefix stripped before the walk; otherwise the first
/// segment never matches and the diagram-decoration layer silently
/// renders nothing.
fn walk_qualified<'a>(
    ast: &'a rumoca_session::parsing::ast::StoredDefinition,
    qualified: &str,
) -> Option<&'a rumoca_session::parsing::ast::ClassDef> {
    let stripped = if let Some(within) = ast.within.as_ref() {
        let prefix = within
            .name
            .iter()
            .map(|t| t.text.as_ref())
            .collect::<Vec<_>>()
            .join(".");
        if !prefix.is_empty() {
            if let Some(rest) = qualified.strip_prefix(&prefix) {
                rest.strip_prefix('.').unwrap_or(rest)
            } else {
                qualified
            }
        } else {
            qualified
        }
    } else {
        qualified
    };
    let mut segments = stripped.split('.');
    let first = segments.next()?;
    let mut current = ast.classes.iter().find(|(n, _)| n.as_str() == first).map(|(_, c)| c)?;
    for seg in segments {
        current = current.classes.get(seg)?;
    }
    Some(current)
}

/// Find a class by short name in the AST — top-level first, then one
/// level of nested classes (the same scope `register_local_class`
/// uses for the Twin Browser).
fn locate_class<'a>(
    ast: &'a rumoca_session::parsing::ast::StoredDefinition,
    name: &str,
) -> Option<&'a rumoca_session::parsing::ast::ClassDef> {
    if let Some((_, c)) = ast.classes.iter().find(|(n, _)| n.as_str() == name) {
        return Some(c);
    }
    for (_, top) in ast.classes.iter() {
        if let Some(c) = top.classes.get(name) {
            return Some(c);
        }
    }
    // Final fallback: first non-package class (matches the workbench's
    // "active class on first open" picker).
    use rumoca_session::parsing::ClassType;
    ast.classes
        .iter()
        .find(|(_, c)| !matches!(c.class_type, ClassType::Package))
        .map(|(_, c)| c)
}

/// Render a row showing a symbol band (e.g. "Parameters: tau, J, c
/// + 3 more"). When the names list is empty, falls through to "—".
fn paint_symbol_band(
    ui: &mut egui::Ui,
    label: &str,
    names: &[String],
    total: usize,
    theme: &lunco_theme::Theme,
) {
    if total == 0 && names.is_empty() {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("{label}:"))
                .small()
                .color(theme.text_muted()),
        );
        let shown = names.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
        let suffix = if total > shown.len() && total > names.len().min(6) && names.len() > 6 {
            format!(" + {} more", total - 6)
        } else {
            String::new()
        };
        let display = if shown.is_empty() {
            format!("({total})")
        } else {
            format!("{shown}{suffix}")
        };
        ui.monospace(
            egui::RichText::new(display)
                .small()
                .color(theme.tokens.accent),
        );
    });
}

/// Stylised type badge used as the hero when a class has no authored
/// `Icon` annotation. A centred coloured pill with a single uppercase
/// letter — matches the [`crate::ui::browser_section`] type-badge
/// palette so the canvas hero and the browser row read as the same
/// "this is a model" affordance.
fn paint_class_type_badge(
    painter: &egui::Painter,
    rect: egui::Rect,
    type_name: &str,
    theme: &lunco_theme::Theme,
) {
    let letter = match type_name {
        "model" => "M",
        "block" => "B",
        "class" => "C",
        "connector" => "X",
        "record" => "R",
        "type" => "T",
        "package" => "P",
        "function" => "F",
        _ => "?",
    };
    let bg = theme.class_badge_bg_by_keyword(type_name);
    let pill_w = rect.width().min(rect.height() * 1.4);
    let pill_h = rect.height().min(120.0);
    let pill = egui::Rect::from_center_size(rect.center(), egui::vec2(pill_w, pill_h));
    painter.rect_filled(pill, 16.0, bg);
    painter.text(
        pill.center(),
        egui::Align2::CENTER_CENTER,
        letter,
        egui::FontId::proportional(pill_h * 0.55),
        theme.class_badge_fg(),
    );
}

/// One-time-compiled regexes used by the empty-diagram summary.
///
/// Previously [`count_matches`] compiled a fresh `Regex` on every
/// call — fine for small user sources, catastrophic for 184 KB
/// drill-ins where the overlay fires 5× per frame at 60 Hz. Cached
/// here via `OnceLock` so compile cost is paid once at first use
/// and the per-frame work collapses to scan-only.
// TODO(index-migrate): replace the regex scan with an Index read.
// The per-doc [`crate::index::ModelicaIndex`] already tracks
// components (variability/causality), connections, and (after
// further growth) equations — counts can be derived directly:
//   - parameters: components_in_class().filter(|c| c.variability == Parameter).count()
//   - inputs:     components_in_class().filter(|c| c.causality == Input).count()
//   - outputs:    components_in_class().filter(|c| c.causality == Output).count()
//   - connects:   connections_in_class().count()
//   - equations:  Index doesn't track algebraic equations yet — needs growth.
// Migrating this saves the 5-pattern-per-frame scan and removes the
// hand-rolled BLAKE3 cache below. Deferred: the existing OnceLock
// cache makes per-frame cost low, and `equations` needs Index growth
// first.
fn empty_overlay_regexes() -> &'static [(&'static str, regex::Regex); 5] {
    use std::sync::OnceLock;
    static RE: OnceLock<[(&str, regex::Regex); 5]> = OnceLock::new();
    RE.get_or_init(|| {
        [
            ("parameters", regex::Regex::new(r"(?m)^\s*parameter\s+").unwrap()),
            ("inputs", regex::Regex::new(r"(?m)^\s*input\s+").unwrap()),
            ("outputs", regex::Regex::new(r"(?m)^\s*output\s+").unwrap()),
            (
                "equations",
                regex::Regex::new(
                    r"(?m)^\s*(?:der\s*\(|[A-Za-z_]\w*\s*=\s*[^=])",
                )
                .unwrap(),
            ),
            (
                "connects",
                regex::Regex::new(r"\bconnect\s*\(").unwrap(),
            ),
        ]
    })
}

/// Counts for the empty-diagram overlay, cached per source so the
/// ~5 regex scans on large MSL files aren't re-run every frame.
/// Key is `(source length, blake3 hash of the first 4 KB)` — cheap
/// to compute, collision rate negligible for this use.
#[derive(Clone, Copy, Default)]
struct EmptyOverlayCounts {
    params: usize,
    inputs: usize,
    outputs: usize,
    equations: usize,
    connects: usize,
}

fn empty_overlay_counts_cached(source: &str) -> EmptyOverlayCounts {
    use std::sync::Mutex;
    use std::sync::OnceLock;
    // Source-len keyed cache is intentionally small (1 slot). The
    // overlay only shows one source at a time per active tab; if
    // two tabs alternate, worst case is we rescan once on switch.
    // Can be promoted to a HashMap keyed by DocumentId if tab
    // switching turns out to be frequent.
    static CACHE: OnceLock<Mutex<Option<(usize, u64, EmptyOverlayCounts)>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let prefix_hash = {
        let mut h: u64 = source.len() as u64;
        for b in source.as_bytes().iter().take(4096) {
            h = h.wrapping_mul(0x100000001b3).wrapping_add(*b as u64);
        }
        h
    };
    if let Ok(guard) = cache.lock() {
        if let Some((len, hash, counts)) = *guard {
            if len == source.len() && hash == prefix_hash {
                return counts;
            }
        }
    }
    let regexes = empty_overlay_regexes();
    let counts = EmptyOverlayCounts {
        params: regexes[0].1.find_iter(source).count(),
        inputs: regexes[1].1.find_iter(source).count(),
        outputs: regexes[2].1.find_iter(source).count(),
        equations: regexes[3].1.find_iter(source).count(),
        connects: regexes[4].1.find_iter(source).count(),
    };
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((source.len(), prefix_hash, counts));
    }
    counts
}

// ─── Drill-in ───────────────────────────────────────────────────────

/// Tab-to-class binding for drill-in tabs whose document hasn't
/// been installed in the registry yet. Keyed by the reserved
/// DocumentId, valued by the qualified class name the tab is
/// waiting on.
///
/// The heavy work (file read + rumoca parse) lives in
/// [`crate::class_cache::ClassCache`]; this resource only tracks
/// which tabs care about which class. When the cache resolves,
/// [`drive_drill_in_loads`] builds a `ModelicaDocument` from the
/// cached AST + source (no second parse) and installs it into the
/// registry, clearing the binding.
///
/// The name `DrillInLoads` is preserved for minimal churn; the
/// resource is effectively "tabs waiting on a class cache entry".
#[derive(bevy::prelude::Resource, Default)]
pub struct DrillInLoads {
    pending: std::collections::HashMap<lunco_doc::DocumentId, DrillInBinding>,
}

/// User-facing canvas snap settings, read each frame by the canvas
/// render path and pushed onto [`lunco_canvas::Canvas::snap`]. Off by
/// default; the Settings menu flips `enabled` and picks the step.
///
/// Step is in Modelica world units (not screen pixels) so the visible
/// grid spacing stays constant across zooms. Typical choices for the
/// standard `{{-100,-100},{100,100}}` diagram coord system:
///   * `2` — fine (matches common MSL placement granularity)
///   * `5` — medium
///   * `10` — coarse (matches typical integer placements in MSL)
#[derive(bevy::prelude::Resource)]
pub struct CanvasSnapSettings {
    pub enabled: bool,
    pub step: f32,
}

impl Default for CanvasSnapSettings {
    fn default() -> Self {
        // On by default. Step = 5 Modelica units — the OMEdit
        // default and the value most MSL example placements are
        // authored to (common placement extents are multiples of 5
        // or 10). Fine enough to reach typical target positions,
        // coarse enough that every drag produces a visibly
        // different "tick" as the icon crosses grid lines.
        Self {
            enabled: true,
            step: 5.0,
        }
    }
}

/// Persistent `DocumentId → qualified class name` map for tabs
/// opened via drill-in. Lives for the tab's lifetime (cleared by
/// [`cleanup_removed_documents`]), so downstream systems — canvas
/// projection, especially — can ask "what class was this tab
/// drilled into?" after install has already cleared the transient
/// [`DrillInLoads`] entry.
///
/// Without this, projection for a drill-in tab can't scope to the
/// specific class: the installed `ModelicaDocument.canonical_path`
/// is the `.mo` file, which for multi-class package files doesn't
/// tell us which of the dozen classes inside the user meant.
#[derive(bevy::prelude::Resource, Default)]
pub struct DrilledInClassNames {
    pub by_doc: std::collections::HashMap<lunco_doc::DocumentId, String>,
}

impl DrilledInClassNames {
    pub fn get(&self, doc: lunco_doc::DocumentId) -> Option<&str> {
        self.by_doc.get(&doc).map(String::as_str)
    }
    pub fn set(&mut self, doc: lunco_doc::DocumentId, qualified: String) {
        self.by_doc.insert(doc, qualified);
    }
    pub fn remove(&mut self, doc: lunco_doc::DocumentId) -> Option<String> {
        self.by_doc.remove(&doc)
    }
}

pub struct DrillInBinding {
    pub qualified: String,
    /// When the tab was opened. Used to show elapsed-seconds in the
    /// loading overlay so the user sees work is happening even when
    /// rumoca takes tens of seconds on large package files.
    pub started: web_time::Instant,
    /// Off-thread document load. Built via
    /// [`crate::document::ModelicaDocument::load_msl_file`] which
    /// hits rumoca's content-hash artifact cache, so a class whose
    /// containing file the engine session has already parsed
    /// installs in milliseconds. Driven by [`drive_drill_in_loads`].
    pub task: bevy::tasks::Task<Result<crate::document::ModelicaDocument, String>>,
}

/// Tab-to-task binding for duplicate-to-workspace operations whose
/// bg parse hasn't finished yet. The parse goes off the UI thread
/// because a naïve `allocate_with_origin` on a multi-KB source
/// re-runs rumoca synchronously — locked the workbench for seconds
/// in debug builds, which users (correctly) called a bug:
/// *"no operations like that must be in UI thread"*.
///
/// Same shape as [`DrillInLoads`]: the bg task returns a fully-built
/// [`ModelicaDocument`], the driver system installs it into the
/// registry via `install_prebuilt`. Cleared on install and on
/// document removal.
#[derive(bevy::prelude::Resource, Default)]
pub struct DuplicateLoads {
    pending: std::collections::HashMap<
        lunco_doc::DocumentId,
        DuplicateBinding,
    >,
}

pub struct DuplicateBinding {
    pub display_name: String,
    pub origin_short: String,
    /// Path *within* the duplicated top class the user was drilled
    /// into when they hit Duplicate. e.g. duplicating an
    /// `AnnotatedRocketStage` package while focused on its inner
    /// `RocketStage` model lands here as `Some("RocketStage")` and
    /// the install hook seeds `DrilledInClassNames` with
    /// `<top_copy>.<inner_drill>` so the new tab opens on that
    /// same inner class. `None` when the user was on the top
    /// class itself.
    pub inner_drill: Option<String>,
    pub started: web_time::Instant,
    pub task: bevy::tasks::Task<crate::document::ModelicaDocument>,
}

impl DuplicateLoads {
    pub fn is_loading(&self, doc: lunco_doc::DocumentId) -> bool {
        self.pending.contains_key(&doc)
    }
    pub fn detail(&self, doc: lunco_doc::DocumentId) -> Option<&str> {
        self.pending.get(&doc).map(|b| b.display_name.as_str())
    }
    pub fn progress(&self, doc: lunco_doc::DocumentId) -> Option<(&str, f32)> {
        self.pending
            .get(&doc)
            .map(|b| (b.display_name.as_str(), b.started.elapsed().as_secs_f32()))
    }
    pub fn insert(
        &mut self,
        doc: lunco_doc::DocumentId,
        binding: DuplicateBinding,
    ) {
        self.pending.insert(doc, binding);
    }
}

impl DrillInLoads {
    pub fn is_loading(&self, doc: lunco_doc::DocumentId) -> bool {
        self.pending.contains_key(&doc)
    }
    pub fn detail(&self, doc: lunco_doc::DocumentId) -> Option<&str> {
        self.pending.get(&doc).map(|b| b.qualified.as_str())
    }
    /// `(qualified, seconds elapsed since tab opened)` for the
    /// loading overlay. Returns `None` if nothing is loading for
    /// this doc.
    pub fn progress(&self, doc: lunco_doc::DocumentId) -> Option<(&str, f32)> {
        self.pending
            .get(&doc)
            .map(|b| (b.qualified.as_str(), b.started.elapsed().as_secs_f32()))
    }
}

/// Bevy system: for each pending drill-in binding, check whether
/// its class has landed in [`ClassCache`]. If yes, build a
/// `ModelicaDocument` from the cached parts (no re-parse) and
/// install it in the registry.
/// Bevy system: poll pending duplicate bg tasks; `install_prebuilt`
/// the fully-built document into the registry when ready. Same
/// shape as [`drive_drill_in_loads`] but for the `Duplicate to
/// Workspace` flow.
pub fn drive_duplicate_loads(
    mut loads: bevy::prelude::ResMut<DuplicateLoads>,
    mut registry: bevy::prelude::ResMut<ModelicaDocumentRegistry>,
    mut probe: Option<bevy::prelude::ResMut<crate::FrameTimeProbe>>,
    mut egui_q: bevy::prelude::Query<&mut bevy_egui::EguiContext>,
    mut class_names: bevy::prelude::ResMut<DrilledInClassNames>,
) {
    use bevy::prelude::*;
    // While any duplicate is in-flight, ping egui every tick so the
    // canvas keeps repainting and the loading overlay actually
    // animates. Without this the canvas paints once at tab-open then
    // sleeps until something else requests a repaint — the overlay
    // is unreachable for the entire bg-parse window and the user sees
    // a blank canvas (verified via [Overlay] trace: no entries between
    // "ModelView rendering tab" and "duplicate: installed").
    if !loads.pending.is_empty() {
        for mut ctx in egui_q.iter_mut() {
            ctx.get_mut().request_repaint();
        }
    }
    let doc_ids: Vec<lunco_doc::DocumentId> = loads.pending.keys().copied().collect();
    let mut had_install = false;
    for doc_id in doc_ids {
        let Some(binding) = loads.pending.get_mut(&doc_id) else {
            continue;
        };
        let t_poll = web_time::Instant::now();
        let Some(doc) = futures_lite::future::block_on(
            futures_lite::future::poll_once(&mut binding.task),
        ) else {
            continue;
        };
        let poll_ms = t_poll.elapsed().as_secs_f64() * 1000.0;
        let dup_display_name = binding.display_name.clone();
        let origin_short = binding.origin_short.clone();
        let inner_drill = binding.inner_drill.clone();
        loads.pending.remove(&doc_id);
        let t_install = web_time::Instant::now();
        registry.install_prebuilt(doc_id, doc);
        let install_ms = t_install.elapsed().as_secs_f64() * 1000.0;
        info!(
            "[CanvasDiagram] duplicate: installed `{}` (from `{}`) — poll={poll_ms:.1}ms install={install_ms:.1}ms",
            dup_display_name, origin_short,
        );
        had_install = true;
        // Seed the drill-in target so the canvas projects the inner
        // model, not the package's empty top-level. Duplicating a
        // `package Foo { model Bar ... }` lands as
        // `package FooCopy { model Bar ... }`; `DrilledInClassNames`
        // points at `FooCopy.Bar` so the projection scopes the builder
        // to that class. Without this the user sees the empty-overlay
        // placeholder card and has to click into the package tree
        // manually.
        if let Some(host) = registry.host(doc_id) {
            // Read first non-package class from the per-doc Index;
            // sees optimistic patches and avoids walking the AST.
            let index = host.document().index();
            let qualified = match inner_drill.as_deref() {
                Some(rest) => Some(format!("{dup_display_name}.{rest}")),
                None => index
                    .classes
                    .values()
                    .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                    .map(|c| c.name.clone()),
            };
            if let Some(q) = qualified {
                class_names.set(doc_id, q);
            }
        }
        // Pre-warm the MSL inheritance chain on a dedicated thread so
        // the projection finds inherited connectors. Same pattern as
        // the drill-in path. The duplicated copy carries `within
        // <origin package>;` so the within-prefixed qualified path
        // (e.g. `Modelica.Blocks.Continuous.PIDCopy`) gives the
        // scope-chain resolver enough context to walk up to
        // `Modelica.Blocks.Interfaces.SISO`.
        if let Some(host) = registry.host(doc_id) {
            // Read within-prefix + extends from the Index. Both are
            // pre-extracted during rebuild, so no AST walk per drill-in.
            let index = host.document().index();
            let within_prefix = index.within_path.clone().unwrap_or_default();
            let qpath = if within_prefix.is_empty() {
                dup_display_name.clone()
            } else {
                format!("{within_prefix}.{dup_display_name}")
            };
            // Fall back to the short name when the qualified path
            // isn't directly indexed (e.g. user-typed un-`within`'d
            // top-level classes).
            let entry = index
                .classes
                .get(&qpath)
                .or_else(|| index.classes.get(&dup_display_name));
            // Engine session caches across calls; the projection task
            // resolves inherited components on demand. No off-thread
            // prewarm needed.
            let _ = entry;
        }
    }
    if had_install {
        if let Some(p) = probe.as_deref_mut() {
            p.last_edit = Some(web_time::Instant::now());
        }
    }
}

pub fn drive_drill_in_loads(
    mut loads: bevy::prelude::ResMut<DrillInLoads>,
    mut registry: bevy::prelude::ResMut<ModelicaDocumentRegistry>,
    mut tabs: bevy::prelude::ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut class_names: bevy::prelude::ResMut<DrilledInClassNames>,
    mut egui_q: bevy::prelude::Query<&mut bevy_egui::EguiContext>,
) {
    use bevy::prelude::*;
    // Keep egui awake while loads are in flight so the "Loading…"
    // overlay actually animates. Mirrors the duplicate-loads driver
    // — without this the canvas paints once and sleeps until input.
    if !loads.pending.is_empty() {
        for mut ctx in egui_q.iter_mut() {
            ctx.get_mut().request_repaint();
        }
    }
    let doc_ids: Vec<lunco_doc::DocumentId> = loads.pending.keys().copied().collect();
    for doc_id in doc_ids {
        let Some(binding) = loads.pending.get_mut(&doc_id) else {
            continue;
        };
        let Some(result) = futures_lite::future::block_on(
            futures_lite::future::poll_once(&mut binding.task),
        ) else {
            continue;
        };
        let qualified = binding.qualified.clone();
        loads.pending.remove(&doc_id);
        let doc = match result {
            Ok(doc) => doc,
            Err(msg) => {
                warn!(
                    "[CanvasDiagram] drill-in: class `{}` load failed: {}",
                    qualified, msg
                );
                continue;
            }
        };
        // Capture file path for the install log + smart-view decision
        // before moving the doc into the registry.
        let (file_path_display, has_components) = {
            let path = match doc.origin() {
                lunco_doc::DocumentOrigin::File { path, .. } => path.display().to_string(),
                _ => String::from("<no path>"),
            };
            // Smart default view for the drilled-in tab. Matches
            // OMEdit/Dymola: icon-only class or class with zero
            // instantiated components → Icon view; otherwise Canvas
            // (the user drilled FROM a canvas, expects a canvas).
            let has_components = doc.ast().ast().and_then(|ast| {
                crate::diagram::find_class_by_qualified_name(ast, &qualified)
                    .map(|c| !c.components.is_empty())
            });
            (path, has_components)
        };
        registry.install_prebuilt(doc_id, doc);
        // Persistent binding so projection can scope to this class
        // after `loads` is cleared — required for multi-class package
        // files where `canonical_path` only tells us the `.mo` file.
        class_names.set(doc_id, qualified.clone());
        let land_in_icon_view =
            crate::ui::loaded_classes::is_icon_only_class(&qualified)
                || has_components == Some(false);
        if land_in_icon_view {
            if let Some(tab) = tabs.get_mut(doc_id) {
                tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Icon;
            }
        }
        info!(
            "[CanvasDiagram] drill-in: installed `{}` from `{}`",
            qualified, file_path_display,
        );
    }
}

/// Open the Modelica class with `qualified` name in a new tab.
/// The tab appears immediately with an empty document showing a
/// "Loading…" overlay; the file read happens on a background task
/// and the source is applied via `ReplaceSource` when the read
/// completes. This matches what users expect: the tab opens, a
/// spinner says "loading", content lands when it's ready.
pub fn drill_into_class(world: &mut World, qualified: &str) {
    // Try MSL paths first (resolves Modelica.* and any other MSL-rooted
    // qualified path). Fallback: scan the open document registry for a
    // doc whose AST contains the requested class — handles non-MSL
    // user-opened files (e.g. `assets/models/AnnotatedRocketStage.mo`)
    // where the qualified name lives only in a workspace document.
    let file_path = crate::library_fs::resolve_class_path_indexed(qualified)
        .or_else(|| crate::library_fs::locate_library_file(qualified));
    if let Some(file_path) = file_path {
        open_drill_in_tab(world, qualified, &file_path);
        return;
    }
    // Open-document fallback: find a host whose parsed AST resolves the
    // qualified path. Reuse its tab + just set the drill-in class.
    let target_doc: Option<lunco_doc::DocumentId> = {
        let registry = world.resource::<ModelicaDocumentRegistry>();
        registry.iter().find_map(|(doc_id, host)| {
            host.document().ast().ast().and_then(|ast| {
                crate::diagram::find_class_by_qualified_name(ast, qualified)
                    .map(|_| doc_id)
            })
        })
    };
    if let Some(doc_id) = target_doc {
        // Switch focus to this doc's tab and record the drilled-in
        // class so the canvas projection scopes itself.
        if let Some(mut tabs) =
            world.get_resource_mut::<crate::ui::panels::model_view::ModelTabs>()
        {
            if let Some(tab) = tabs.get_mut(doc_id) {
                tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
            }
        }
        if let Some(mut names) =
            world.get_resource_mut::<DrilledInClassNames>()
        {
            names.set(doc_id, qualified.to_string());
        }
        if let Some(mut workspace) =
            world.get_resource_mut::<lunco_workbench::WorkspaceResource>()
        {
            workspace.active_document = Some(doc_id);
        }
        bevy::log::info!(
            "[CanvasDiagram] drill-in: focused open doc for `{}`",
            qualified,
        );
        return;
    }
    bevy::log::warn!(
        "[CanvasDiagram] drill-in: could not locate `{}` (no MSL match, no open doc with that class)",
        qualified
    );
}

/// Open a tab for `qualified` class backed by a **placeholder
/// document** — empty source, parses instantly. Spawns a bg task
/// that reads the file; a later Bevy system applies `ReplaceSource`
/// when the read completes.
///
/// The user sees:
///  1. Instant: a new tab titled with the class short name.
///  2. Immediately: an "Loading…" overlay on the canvas.
///  3. A moment later: the real source + diagram populates.
///
/// If a tab for the same file path is already open (from a
/// previous drill-in), we focus it instead of making a second.
fn open_drill_in_tab(
    world: &mut World,
    qualified: &str,
    file_path: &std::path::Path,
) {
    // Find or allocate the doc. Reuse an existing one only if the
    // same `(file, drilled-in class)` was opened before — keying on
    // file alone collapsed sibling MSL classes (e.g. `Integrator`
    // and `Derivative` both in `Continuous.mo`) onto one tab, so a
    // second drill silently focused the first tab instead of
    // showing the requested class.
    let model_path_id = format!("msl://{qualified}");
    let existing_doc = {
        let registry = world.resource::<ModelicaDocumentRegistry>();
        let tabs = world.resource::<crate::ui::panels::model_view::ModelTabs>();
        let class_names = world.resource::<DrilledInClassNames>();
        tabs.iter_docs().find(|&doc_id| {
            let same_file = registry
                .host(doc_id)
                .and_then(|h| match h.document().origin() {
                    lunco_doc::DocumentOrigin::File { path, .. } => {
                        Some(path == file_path)
                    }
                    _ => None,
                })
                .unwrap_or(false);
            same_file
                && class_names
                    .get(doc_id)
                    .map(|n| n == qualified)
                    .unwrap_or(false)
        })
    };
    let (doc_id, needs_load) = if let Some(id) = existing_doc {
        (id, false)
    } else {
        // Reserve a doc id only; the actual `ModelicaDocument`
        // (including the rumoca parse) is built on a background
        // thread and installed via `install_prebuilt` when ready.
        // Queries against the id before install return `None` —
        // panels render the "Loading resource…" overlay based on
        // `DrillInLoads::is_loading`.
        let mut registry = world.resource_mut::<ModelicaDocumentRegistry>();
        let id = registry.reserve_id();
        (id, true)
    };

    if needs_load {
        // Spawn the off-thread load. `ModelicaDocument::load_msl_file`
        // routes through rumoca's content-hash artifact cache, so
        // every class drilled into the same file (Continuous.mo holds
        // Der/Integrator/PID/...) shares one parse the second time
        // around. Driver: `drive_drill_in_loads`.
        let path_for_task = file_path.to_path_buf();
        let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            crate::document::ModelicaDocument::load_msl_file(doc_id, &path_for_task)
        });
        let mut loads = world.resource_mut::<DrillInLoads>();
        loads.pending.insert(
            doc_id,
            DrillInBinding {
                qualified: qualified.to_string(),
                started: web_time::Instant::now(),
                task,
            },
        );
    }
    // Bind the drilled-in class eagerly — without this, a second
    // drill into a sibling class in the same file would race the
    // (post-install) `class_names.set` and find no class binding,
    // letting the file-level dedup re-fire and steal the tab.
    {
        let mut class_names = world.resource_mut::<DrilledInClassNames>();
        class_names.set(doc_id, qualified.to_string());
    }

    let _ = model_path_id;

    // Register the tab + land the user in Canvas view (they
    // drilled FROM a canvas, so the canvas is what they expect
    // to see). Default `view_mode` is Text for newly-created
    // scratch models; drill-in is a different use case.
    {
        let mut model_tabs =
            world.resource_mut::<crate::ui::panels::model_view::ModelTabs>();
        model_tabs.ensure(doc_id);
        if let Some(tab) = model_tabs.get_mut(doc_id) {
            tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        }
    }
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: doc_id.raw(),
    });

    bevy::log::info!(
        "[CanvasDiagram] drill-in: opened placeholder tab for `{}` (file: `{}`) — loading in background",
        qualified,
        file_path.display()
    );
}

// ─── Doc-op translation ─────────────────────────────────────────────

/// Resolve `(document id, editing class name)` for the current tab.
/// Used by the canvas + neighbours so they target the same class when
/// `open_model` is bound.
fn resolve_doc_context(world: &World) -> (Option<lunco_doc::DocumentId>, Option<String>) {
    // Active doc from the Workspace session; `open_model.detected_name`
    // is read as a display-cache fallback when the registry AST hasn't
    // caught up yet. Both paths are optional — the caller tolerates
    // `(None, None)` by deferring.
    let Some(doc_id) = world
        .resource::<lunco_workbench::WorkspaceResource>()
        .active_document
    else {
        return (None, None);
    };
    // Class resolution priority — must match `compile_model`'s logic
    // and `active_class_for_doc` so the canvas's *edit* target lines
    // up with what compile / projection consider authoritative:
    //   1. drilled-in pin (user explicitly navigated into a class)
    //   2. first non-package class via `extract_model_name_from_ast`
    //   3. `WorkbenchState.open_model.detected_name` (display cache)
    //
    // The previous `s.classes.keys().next()` returned the IndexMap's
    // first key, which for a multi-class file wrapped in a `package`
    // (AnnotatedRocketStage, every MSL example, …) is the *package*
    // wrapper. Adding a component to a package corrupts the file —
    // packages can only contain classes, not components.
    let drilled_in = world
        .get_resource::<DrilledInClassNames>()
        .and_then(|m| m.get(doc_id).map(str::to_string));
    let open = world.resource::<WorkbenchState>().open_model.as_ref();
    let class = drilled_in
        .or_else(|| {
            world
                .resource::<ModelicaDocumentRegistry>()
                .host(doc_id)
                .and_then(|h| {
                    h.document()
                        .ast()
                        .ast()
                        .and_then(crate::ast_extract::extract_model_name_from_ast)
                })
        })
        .or_else(|| open.and_then(|o| o.detected_name.clone()));
    (Some(doc_id), class)
}

// Thin wrapper so existing call sites keep their shape. The real
// conversion lives in `coords::canvas_min_to_modelica_center`.
fn canvas_min_to_modelica_center(min: lunco_canvas::Pos) -> (f32, f32) {
    let m = coords::canvas_min_to_modelica_center(min, ICON_W, ICON_H);
    (m.x, m.y)
}

/// Translate canvas scene events into ModelicaOps. Needs a brief
/// read-only borrow of the scene (to look up edge endpoints); the
/// caller runs it inside its own borrow scope.
fn build_ops_from_events(
    world: &mut World,
    events: &[lunco_canvas::SceneEvent],
    class: &str,
) -> Vec<ModelicaOp> {
    use lunco_canvas::SceneEvent;
    let active_doc = active_doc_from_world(world);
    let state = world.resource::<CanvasDiagramState>();
    let scene = &state.get(active_doc).canvas.scene;
    let mut ops: Vec<ModelicaOp> = Vec::new();

    for ev in events {
        match ev {
            SceneEvent::NodeMoved { id, new_min, .. } => {
                let Some(node) = scene.node(*id) else { continue };
                // Plot tiles are vendor-annotation rows in
                // `Diagram(graphics)`, not component placements. They
                // round-trip through `SetPlotNodeExtent` keyed by
                // signal path; the on-screen rect is taken straight
                // from `node.rect` (canvas world coords match the
                // Modelica diagram coord system). Identification:
                // origin format is `"plot:<idx>:<signal>"` — split
                // off the signal to use as the op key.
                if node.kind == lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND {
                    let signal = node
                        .origin
                        .as_deref()
                        .and_then(|o| o.strip_prefix("plot:"))
                        .and_then(|rest| rest.split_once(':').map(|(_, s)| s.to_string()))
                        .or_else(|| {
                            // Fallback for legacy / scratch plot
                            // nodes whose origin isn't in the source
                            // form yet — pull the signal out of the
                            // node's `data` payload.
                            node.data
                                .downcast_ref::<lunco_viz::kinds::canvas_plot_node::PlotNodeData>()
                                .map(|d| d.signal_path.clone())
                        });
                    let Some(signal_path) = signal.filter(|s| !s.is_empty()) else {
                        continue;
                    };
                    let w = node.rect.width().max(1.0);
                    let h = node.rect.height().max(1.0);
                    ops.push(ModelicaOp::SetPlotNodeExtent {
                        class: class.to_string(),
                        signal_path,
                        x1: new_min.x,
                        y1: new_min.y,
                        x2: new_min.x + w,
                        y2: new_min.y + h,
                    });
                    continue;
                }
                if node.kind == crate::ui::text_node::TEXT_NODE_KIND {
                    let Some(idx) = node
                        .origin
                        .as_deref()
                        .and_then(|o| o.strip_prefix("text:"))
                        .and_then(|n| n.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    let w = node.rect.width().max(1.0);
                    let h = node.rect.height().max(1.0);
                    // Canvas → Modelica: negate Y so the source
                    // sees +Y up and the round-trip is stable
                    // (re-projection emits the same screen rect).
                    ops.push(ModelicaOp::SetDiagramTextExtent {
                        class: class.to_string(),
                        index: idx,
                        x1: new_min.x,
                        y1: -new_min.y,
                        x2: new_min.x + w,
                        y2: -(new_min.y + h),
                    });
                    continue;
                }
                // The `origin` we set during projection carries the
                // Modelica instance name. Skip if missing (shouldn't
                // happen — projection always sets it).
                let Some(name) = node.origin.clone() else { continue };
                // Use the node's actual icon extent — `Placement::at`
                // hardcodes 20×20, which silently shrinks (or grows)
                // every dragged component back to the default size on
                // re-projection. Read the live `node.rect` instead so
                // the new placement preserves whatever size the icon
                // already has on screen (canvas world coords are 1:1
                // with Modelica units, just Y-flipped).
                let icon_w = node.rect.width().max(1.0);
                let icon_h = node.rect.height().max(1.0);
                let m = coords::canvas_min_to_modelica_center(*new_min, icon_w, icon_h);
                ops.push(ModelicaOp::SetPlacement {
                    class: class.to_string(),
                    name,
                    placement: Placement {
                        x: m.x,
                        y: m.y,
                        width: icon_w,
                        height: icon_h,
                    },
                });
            }
            SceneEvent::NodeResized { id, new_rect, .. } => {
                let Some(node) = scene.node(*id) else { continue };
                if node.kind == lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND {
                    let signal = node
                        .origin
                        .as_deref()
                        .and_then(|o| o.strip_prefix("plot:"))
                        .and_then(|rest| rest.split_once(':').map(|(_, s)| s.to_string()))
                        .or_else(|| {
                            node.data
                                .downcast_ref::<lunco_viz::kinds::canvas_plot_node::PlotNodeData>()
                                .map(|d| d.signal_path.clone())
                        });
                    let Some(signal_path) = signal.filter(|s| !s.is_empty()) else {
                        continue;
                    };
                    ops.push(ModelicaOp::SetPlotNodeExtent {
                        class: class.to_string(),
                        signal_path,
                        x1: new_rect.min.x,
                        y1: new_rect.min.y,
                        x2: new_rect.max.x,
                        y2: new_rect.max.y,
                    });
                    continue;
                }
                if node.kind == crate::ui::text_node::TEXT_NODE_KIND {
                    let Some(idx) = node
                        .origin
                        .as_deref()
                        .and_then(|o| o.strip_prefix("text:"))
                        .and_then(|n| n.parse::<usize>().ok())
                    else {
                        continue;
                    };
                    ops.push(ModelicaOp::SetDiagramTextExtent {
                        class: class.to_string(),
                        index: idx,
                        x1: new_rect.min.x,
                        y1: -new_rect.min.y,
                        x2: new_rect.max.x,
                        y2: -new_rect.max.y,
                    });
                    continue;
                }
                // Component icon resize → `SetPlacement` keeping
                // the node's centre fixed but adopting the new
                // width/height. Lets users tighten oversized library
                // icons on the canvas without writing source by hand.
                let Some(name) = node.origin.clone() else { continue };
                let w = new_rect.width().max(1.0);
                let h = new_rect.height().max(1.0);
                let m = coords::canvas_min_to_modelica_center(new_rect.min, w, h);
                ops.push(ModelicaOp::SetPlacement {
                    class: class.to_string(),
                    name,
                    placement: Placement {
                        x: m.x,
                        y: m.y,
                        width: w,
                        height: h,
                    },
                });
            }
            SceneEvent::EdgeCreated { from, to } => {
                // Resolve canvas port refs → Modelica (instance,
                // port) pairs via node.origin + port.id.
                let Some(from_node) = scene.node(from.node) else { continue };
                let Some(to_node) = scene.node(to.node) else { continue };
                let Some(from_instance) = from_node.origin.clone() else { continue };
                let Some(to_instance) = to_node.origin.clone() else { continue };
                ops.push(ModelicaOp::AddConnection {
                    class: class.to_string(),
                    eq: pretty::ConnectEquation {
                        from: pretty::PortRef::new(&from_instance, from.port.as_str()),
                        to: pretty::PortRef::new(&to_instance, to.port.as_str()),
                        line: None,
                    },
                });
            }
            SceneEvent::EdgeDeleted { id } => {
                if let Some(op) = op_remove_edge_inner(scene, *id, class) {
                    ops.push(op);
                }
            }
            SceneEvent::NodeDeleted { id, orphaned_edges } => {
                // Orphan edge RemoveConnection ops must go in
                // BEFORE the RemoveComponent so rumoca still sees
                // the edges while resolving the connect(...) spans.
                for eid in orphaned_edges {
                    if let Some(op) = op_remove_edge_inner(scene, *eid, class) {
                        ops.push(op);
                    }
                }
                if let Some(op) = op_remove_node_inner(scene, *id, class) {
                    ops.push(op);
                }
            }
            _ => {}
        }
    }
    ops
}

/// `(instance_name, type_label)` for a node, pulled from the scene's
/// `label` + `data.type`. Empty strings when the node is gone.
fn component_headers(
    world: &World,
    id: lunco_canvas::NodeId,
) -> (String, String) {
    let active_doc = active_doc_from_world(world);
    let state = world.resource::<CanvasDiagramState>();
    let Some(node) = state.get(active_doc).canvas.scene.node(id) else {
        return (String::new(), String::new());
    };
    let instance = node.label.clone();
    let type_name = node
        .data
        .downcast_ref::<IconNodeData>()
        .map(|d| d.qualified_type.clone())
        .unwrap_or_default();
    (instance, type_name)
}

/// Pick the next free instance name in `scene` for `comp`. First
/// letter of the short class name + smallest unused integer (`R1`,
/// `R2`, …). Walks `scene.nodes()` directly so the choice respects
/// nodes the user has just optimistically synthesised but that
/// haven't yet round-tripped through the AST.
fn pick_add_instance_name(comp: &MSLComponentDef, scene: &lunco_canvas::Scene) -> String {
    let prefix = comp
        .name
        .chars()
        .next()
        .unwrap_or('X')
        .to_ascii_uppercase();
    let mut n: u32 = 1;
    loop {
        let candidate = format!("{prefix}{n}");
        let taken = scene
            .nodes()
            .any(|(_, node)| node.origin.as_deref() == Some(candidate.as_str()));
        if !taken {
            return candidate;
        }
        n += 1;
    }
}

/// Build an `AddComponent` op at a world-space position with a
/// caller-chosen instance name. Carries the component's default
/// parameter values and a `Placement` annotation so the new node
/// lands at the right spot in both the source and any downstream
/// re-projection.
fn op_add_component_with_name(
    comp: &MSLComponentDef,
    instance_name: &str,
    at_world: lunco_canvas::Pos,
    class: &str,
) -> ModelicaOp {
    let ModelicaPos { x: mx, y: my } = canvas_to_modelica(at_world);
    ModelicaOp::AddComponent {
        class: class.to_string(),
        decl: pretty::ComponentDecl {
            type_name: comp.msl_path.clone(),
            name: instance_name.to_string(),
            modifications: comp
                .parameters
                .iter()
                .filter(|p| !p.default.is_empty())
                .map(|p| (p.name.clone(), p.default.clone()))
                .collect(),
            placement: Some(Placement::at(mx, my)),
        },
    }
}

/// Optimistically synthesise a canvas Node for a freshly-added MSL
/// component, mirroring the subset of [`project_scene`]'s logic that
/// applies before the AST settles. Uses the identity icon transform
/// and the fallback port layout — the next reproject (if any) will
/// replace this with the canonical projection.
///
/// Returns the fresh `NodeId`; the caller pairs it with the matching
/// `AddComponent` op so the optimistic scene + the source rewrite
/// stay in lock-step.
fn synthesize_msl_node(
    scene: &mut lunco_canvas::Scene,
    comp: &MSLComponentDef,
    instance_name: &str,
    at_world: lunco_canvas::Pos,
) -> lunco_canvas::NodeId {
    use lunco_canvas::{Node as CanvasNode, Port as CanvasPort, PortId as CanvasPortId, Pos as CanvasPos, Rect as CanvasRect};

    // Match `Placement::at` — 20×20 canvas units centred on the
    // click. The source rewrite emits the same extent so the
    // canonical reproject (when one happens) keeps the size stable.
    // Using the full -100..100 default would render a node 10× too
    // large compared with what the AST will produce.
    let half = 10.0_f32;
    let icon_w = half * 2.0;
    let icon_h = half * 2.0;
    let min_wx = at_world.x - half;
    let min_wy = at_world.y - half;

    let n_ports = comp.ports.len();
    let ports: Vec<CanvasPort> = comp
        .ports
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (lx, ly) = if p.x == 0.0 && p.y == 0.0 {
                port_fallback_offset_for_size(i, n_ports, icon_w, icon_h)
            } else {
                // Map port coords (-100..100, +Y up) into the
                // 20×20 icon-local screen box (+Y down). Same scale
                // factor 20/200 = 0.1 the projector uses for a
                // Placement::at extent.
                let scale = icon_w / 200.0;
                ((p.x + 100.0) * scale, (100.0 - p.y) * scale)
            };
            CanvasPort {
                id: CanvasPortId::new(p.name.clone()),
                local_offset: CanvasPos::new(lx, ly),
                kind: port_kind_str(p.kind).into(),
            }
        })
        .collect();

    let id = scene.alloc_node_id();
    scene.insert_node(CanvasNode {
        id,
        rect: CanvasRect::from_min_size(CanvasPos::new(min_wx, min_wy), icon_w, icon_h),
        kind: "modelica.icon".into(),
        data: std::sync::Arc::new(IconNodeData {
            qualified_type: comp.msl_path.clone(),
            icon_only: crate::ui::loaded_classes::is_icon_only_class(&comp.msl_path),
            expandable_connector: comp.is_expandable_connector,
            icon_graphics: comp.icon_graphics.clone(),
            diagram_graphics: if comp.class_kind == "connector" {
                comp.diagram_graphics.clone()
            } else {
                None
            },
            rotation_deg: 0.0,
            mirror_x: false,
            mirror_y: false,
            instance_name: instance_name.to_string(),
            parameters: comp
                .parameters
                .iter()
                .map(|p| (p.name.clone(), p.default.clone()))
                .collect(),
            port_connector_paths: comp
                .ports
                .iter()
                .map(|p| (p.name.clone(), p.msl_path.clone(), p.size_x, p.size_y, p.rotation_deg))
                .collect(),
            is_conditional: false,
        }),
        ports,
        label: instance_name.to_string(),
        origin: Some(instance_name.to_string()),
        resizable: false,
        // Optimistic synth: skip the icon-bbox computation (cheap but
        // not free); the next reproject from the source overwrites
        // this node anyway with the bbox-aware version.
        visual_rect: None,
    });
    id
}

fn op_remove_component(
    world: &mut World,
    id: lunco_canvas::NodeId,
    class: &str,
) -> Option<ModelicaOp> {
    let active_doc = active_doc_from_world(world);
    let state = world.resource::<CanvasDiagramState>();
    op_remove_node_inner(&state.get(active_doc).canvas.scene, id, class)
}

fn op_remove_edge(
    world: &mut World,
    id: lunco_canvas::EdgeId,
    class: &str,
) -> Option<ModelicaOp> {
    let active_doc = active_doc_from_world(world);
    let state = world.resource::<CanvasDiagramState>();
    op_remove_edge_inner(&state.get(active_doc).canvas.scene, id, class)
}

fn op_remove_node_inner(
    scene: &lunco_canvas::Scene,
    id: lunco_canvas::NodeId,
    class: &str,
) -> Option<ModelicaOp> {
    let node = scene.node(id)?;
    // Plot tiles delete via `RemovePlotNode` keyed by signal path,
    // not `RemoveComponent` which targets a Modelica component
    // declaration. Same dispatch shape as the move handler above.
    if node.kind == lunco_viz::kinds::canvas_plot_node::PLOT_NODE_KIND {
        let signal_path = node
            .origin
            .as_deref()
            .and_then(|o| o.strip_prefix("plot:"))
            .and_then(|rest| rest.split_once(':').map(|(_, s)| s.to_string()))
            .or_else(|| {
                node.data
                    .downcast_ref::<lunco_viz::kinds::canvas_plot_node::PlotNodeData>()
                    .map(|d| d.signal_path.clone())
            })
            .filter(|s| !s.is_empty())?;
        return Some(ModelicaOp::RemovePlotNode {
            class: class.to_string(),
            signal_path,
        });
    }
    if node.kind == crate::ui::text_node::TEXT_NODE_KIND {
        let idx = node
            .origin
            .as_deref()
            .and_then(|o| o.strip_prefix("text:"))
            .and_then(|n| n.parse::<usize>().ok())?;
        return Some(ModelicaOp::RemoveDiagramText {
            class: class.to_string(),
            index: idx,
        });
    }
    let name = node.origin.clone()?;
    Some(ModelicaOp::RemoveComponent {
        class: class.to_string(),
        name,
    })
}

fn op_remove_edge_inner(
    scene: &lunco_canvas::Scene,
    id: lunco_canvas::EdgeId,
    class: &str,
) -> Option<ModelicaOp> {
    let edge = scene.edge(id)?;
    let from_node = scene.node(edge.from.node)?;
    let to_node = scene.node(edge.to.node)?;
    let from_instance = from_node.origin.clone()?;
    let to_instance = to_node.origin.clone()?;
    Some(ModelicaOp::RemoveConnection {
        class: class.to_string(),
        from: pretty::PortRef::new(&from_instance, edge.from.port.as_str()),
        to: pretty::PortRef::new(&to_instance, edge.to.port.as_str()),
    })
}

/// Apply a batch of ops against the bound document. Ops that fail
/// (e.g. RemoveComponent when the instance isn't actually in source
/// — shouldn't happen, but defence in depth) are logged and
/// skipped. After success the doc's generation bumps, which the
/// next frame picks up via `last_seen_gen` and re-projects.
/// Public re-export of the canvas's op applier so reflect-registered
/// commands (`MoveComponent`, etc.) can dispatch the same SetPlacement
/// pipeline the mouse drag uses — keeps undo/redo + source rewriting
/// consistent across UI-driven and API-driven edits.
pub fn apply_ops_public(
    world: &mut World,
    doc_id: lunco_doc::DocumentId,
    ops: Vec<ModelicaOp>,
) {
    apply_ops(world, doc_id, ops);
}

fn apply_ops(world: &mut World, doc_id: lunco_doc::DocumentId, ops: Vec<ModelicaOp>) {
    // TEMP: timing instrumentation to find the source of the
    // multi-second lag observed when adding components from the
    // right-click menu. Each phase is timed independently so we
    // know which one to optimise.
    let t_start = web_time::Instant::now();
    // Stamp the post-apply window so the canvas frame logger
    // captures every subsequent frame's timing for ~2 seconds.
    if let Ok(mut guard) = LAST_APPLY_AT.lock() {
        *guard = Some(t_start);
    }
    // Stamp the GLOBAL frame-time probe so every Bevy Update tick
    // (not just canvas render) gets logged for the next 5 seconds —
    // catches main-thread blocks anywhere in the schedule.
    crate::frame_time_probe_stamp_edit(world);
    let n = ops.len();
    let mut any_applied = false;
    let mut hit_read_only = false;

    // Preload any newly-referenced MSL class on a background task
    // so the engine session is warm by the time the projection
    // re-runs. Fire-and-forget; rumoca's content-hash artifact
    // cache dedupes repeated requests for the same file.
    for op in &ops {
        if let ModelicaOp::AddComponent { decl, .. } = op {
            if decl.type_name.starts_with("Modelica.") {
                let qualified = decl.type_name.clone();
                bevy::tasks::AsyncComputeTaskPool::get()
                    .spawn(async move {
                        let _ = crate::class_cache::peek_or_load_msl_class(&qualified);
                    })
                    .detach();
            }
        }
    }
    let preload_ms = 0.0_f64;

    let t_apply_start = web_time::Instant::now();
    {
        let Some(mut registry) = world.get_resource_mut::<ModelicaDocumentRegistry>() else {
            bevy::log::warn!(
                "[CanvasDiagram] tried to apply {} op(s) but registry missing",
                n
            );
            return;
        };
        let Some(host) = registry.host_mut(doc_id) else {
            bevy::log::warn!(
                "[CanvasDiagram] tried to apply {} op(s) but doc {:?} not in registry",
                n,
                doc_id
            );
            return;
        };
        for op in ops {
            bevy::log::info!("[CanvasDiagram] applying {:?}", op);
            // Layer 2 authoring ops can resolve a class that a previous
            // op in the same batch just created. The AST cache is
            // debounced (refresh deferred until idle), so the resolver
            // would see a stale snapshot and reject the second op.
            // Force a synchronous reparse for the structural ops where
            // a stale AST means a wrong insertion point or a spurious
            // "class not found" error. Cheap on small docs (a few ms);
            // canvas-driven AddComponent batches don't trip this branch.
            let needs_fresh_ast = matches!(
                &op,
                ModelicaOp::AddClass { .. }
                    | ModelicaOp::RemoveClass { .. }
                    | ModelicaOp::AddShortClass { .. }
                    | ModelicaOp::AddVariable { .. }
                    | ModelicaOp::RemoveVariable { .. }
                    | ModelicaOp::AddEquation { .. }
                    | ModelicaOp::AddIconGraphic { .. }
                    | ModelicaOp::AddDiagramGraphic { .. }
                    | ModelicaOp::SetExperimentAnnotation { .. }
                    | ModelicaOp::ReplaceSource { .. }
            );
            if needs_fresh_ast {
                host.document_mut().refresh_ast_now();
            }
            match host.apply(op) {
                Ok(_) => any_applied = true,
                Err(lunco_doc::Reject::ReadOnly) => {
                    // Document layer rejects mutations on read-only
                    // origins (MSL drill-in, bundled library). We
                    // surface ONE banner per op-batch instead of
                    // spamming per op.
                    hit_read_only = true;
                }
                Err(e) => bevy::log::warn!("[CanvasDiagram] op failed: {}", e),
            }
        }
        // Structured edit batch is one discrete commit — bypass the
        // typing-debounce so the next ast_refresh tick reparses
        // immediately. Otherwise canvas/diagnostics lag 2.5 s behind
        // every API-driven or canvas-drag mutation.
        if any_applied {
            host.document_mut().waive_ast_debounce();
        }
    }
    let apply_ms = t_apply_start.elapsed().as_secs_f64() * 1000.0;

    if hit_read_only {
        if let Some(mut ws) = world.get_resource_mut::<WorkbenchState>() {
            // Don't clobber a real compile error.
            if ws.compilation_error.is_none() {
                ws.compilation_error = Some(
                    "Read-only library tab — edits rejected. \
                     Use File → Duplicate to Workspace to create an \
                     editable copy."
                        .to_string(),
                );
            }
        }
    }

    if !any_applied {
        bevy::log::info!(
            "[CanvasDiagram] apply_ops timing (NO-OP): preload={:.1}ms apply={:.1}ms total={:.1}ms",
            preload_ms,
            apply_ms,
            t_start.elapsed().as_secs_f64() * 1000.0
        );
        return;
    }

    let t_mirror_start = web_time::Instant::now();
    // Mirror the post-edit source back to `WorkbenchState.open_model`
    // so every other panel (code editor, breadcrumb, inspector)
    // that reads the cached source sees the update immediately —
    // the code editor doesn't watch the registry directly; it
    // reads the `Arc<str>` on `open_model`.
    let fresh = world
        .get_resource::<ModelicaDocumentRegistry>()
        .and_then(|r| r.host(doc_id))
        .map(|h| {
            (
                h.document().source().to_string(),
                <crate::document::ModelicaDocument as lunco_doc::Document>::generation(
                    h.document(),
                ),
            )
        });
    if let Some((src, new_gen)) = fresh {
        if let Some(mut ws) = world.get_resource_mut::<WorkbenchState>() {
            if let Some(open) = ws.open_model.as_mut() {
                let mut line_starts = vec![0usize];
                for (i, b) in src.as_bytes().iter().enumerate() {
                    if *b == b'\n' {
                        line_starts.push(i + 1);
                    }
                }
                open.source = std::sync::Arc::from(src.as_str());
                open.line_starts = line_starts.into();
                open.cached_galley = None;
            }
        }
        // Canvas-originated edits have *already* mutated the scene
        // before reaching apply_ops (drag moved the node; menu Add
        // synthesised a node prior to dispatch). Acknowledging the
        // new generation here tells the project gate "the scene
        // already reflects this state — don't re-project". The
        // hash bump keeps the cheap-skip path in `project_now`
        // consistent for any later foreign edit comparison.
        let new_hash = projection_relevant_source_hash(&src);
        if let Some(mut state) = world.get_resource_mut::<CanvasDiagramState>() {
            let docstate = state.get_mut(Some(doc_id));
            docstate.canvas_acked_gen = new_gen;
            docstate.last_seen_gen = new_gen;
            docstate.last_seen_source_hash = new_hash;
        }
    }

    let mirror_ms = t_mirror_start.elapsed().as_secs_f64() * 1000.0;

    // Wake egui. Without this, the canvas panel's `render` only
    // fires on the next input event, so the projection task that
    // would materialise the new component sits idle for whatever
    // egui's reactive sleep happens to be (~2 s in practice). The
    // panel's render pass is what *spawns* the projection task and
    // *polls* the in-flight task — both gated on render running.
    // Pinging every EguiContext component (one per window) brings
    // the next paint within ~16ms, the projection cycle wakes up,
    // and the right-click → component-appears latency drops from
    // multi-second to imperceptible.
    let t_repaint_start = web_time::Instant::now();
    let mut q = world.query::<&mut bevy_egui::EguiContext>();
    for mut ctx in q.iter_mut(world) {
        ctx.get_mut().request_repaint();
    }
    let repaint_ms = t_repaint_start.elapsed().as_secs_f64() * 1000.0;

    bevy::log::info!(
        "[CanvasDiagram] apply_ops timing: preload={:.1}ms apply={:.1}ms mirror={:.1}ms repaint={:.1}ms total={:.1}ms",
        preload_ms,
        apply_ms,
        mirror_ms,
        repaint_ms,
        t_start.elapsed().as_secs_f64() * 1000.0
    );
}

/// Observer for [`crate::ui::commands::AutoArrangeDiagram`].
///
/// Assigns every component of the active class a grid position from
/// the current [`crate::ui::panels::canvas_projection::DiagramAutoLayoutSettings`]
/// `arrange_*` parameters and emits a batch of `SetPlacement` ops.
///
/// Iterates the canvas scene (not the AST) so the order matches what
/// the user sees. Each op is separately undo-able via Ctrl+Z.
pub fn on_auto_arrange_diagram(
    trigger: On<crate::ui::commands::AutoArrangeDiagram>,
    mut commands: Commands,
) {
    let raw = trigger.event().doc;
    // Observers can't take `&mut World` in Bevy 0.18. Defer the real
    // work to an exclusive command — same mutations, just queued to
    // run at the next command-flush boundary.
    commands.queue(move |world: &mut World| {
        // `doc = 0` = API / script default = "the tab the user is
        // looking at right now". Resolve from `WorkbenchState.open_model`
        // so the LunCo API can fire the command without tracking ids.
        let doc_id = if raw.is_unassigned() {
            match active_doc_from_world(world) {
                Some(d) => d,
                None => {
                    bevy::log::warn!(
                        "[CanvasDiagram] Auto-Arrange: no active doc"
                    );
                    return;
                }
            }
        } else {
            raw
        };
        auto_arrange_now(world, doc_id);
    });
}

fn auto_arrange_now(world: &mut World, doc_id: lunco_doc::DocumentId) {
    let Some(class) = active_class_for_doc(world, doc_id) else {
        return;
    };
    let layout = world
        .get_resource::<crate::ui::panels::canvas_projection::DiagramAutoLayoutSettings>()
        .cloned()
        .unwrap_or_default();
    // Capture each node's `origin` (Modelica instance name) AND
    // its existing rect size so Auto-Arrange can preserve per-node
    // extents — the prior `Placement::at` form squashed every icon
    // back to the default 20×20, undoing the user's authored sizes.
    let mut named_with_size: Vec<(String, f32, f32)> = {
        let Some(state) = world.get_resource::<CanvasDiagramState>() else {
            return;
        };
        let docstate = state.get(Some(doc_id));
        docstate
            .canvas
            .scene
            .nodes()
            .filter_map(|(_, n)| {
                let origin = n.origin.clone()?;
                Some((origin, n.rect.width().max(1.0), n.rect.height().max(1.0)))
            })
            .collect()
    };
    // Stable sort + dedup by name: the original `dedup()` only
    // removed adjacent duplicates, which the unsorted scene order
    // didn't guarantee.
    named_with_size.sort_by(|a, b| a.0.cmp(&b.0));
    named_with_size.dedup_by(|a, b| a.0 == b.0);
    if named_with_size.is_empty() {
        return;
    }

    let cols = layout.cols.max(1);
    let dx = layout.spacing_x;
    let dy = layout.spacing_y;
    let stagger = dx * layout.row_stagger;
    let ops: Vec<ModelicaOp> = named_with_size
        .into_iter()
        .enumerate()
        .map(|(idx, (name, w, h))| {
            let row = idx / cols;
            let col = idx % cols;
            let row_shift = if row % 2 == 1 { stagger } else { 0.0 };
            // Canvas world coords (+Y down). Convert to Modelica
            // centre (+Y up) via the shared helper so the ops emit
            // the same coord frame a drag would.
            let wx = col as f32 * dx + row_shift;
            let wy = row as f32 * dy;
            let m = coords::canvas_min_to_modelica_center(
                lunco_canvas::Pos::new(wx, wy),
                w,
                h,
            );
            ModelicaOp::SetPlacement {
                class: class.clone(),
                name,
                placement: Placement {
                    x: m.x,
                    y: m.y,
                    width: w,
                    height: h,
                },
            }
        })
        .collect();
    if ops.is_empty() {
        return;
    }
    bevy::log::info!(
        "[CanvasDiagram] Auto-Arrange: emitting {} SetPlacement ops",
        ops.len()
    );
    #[cfg(feature = "lunco-api")]
    crate::api_edits::trigger_apply_ops(world, doc_id, ops);
    #[cfg(not(feature = "lunco-api"))]
    apply_ops(world, doc_id, ops);
}

/// Resolve the active class name for an Auto-Arrange target. Prefers
/// the drilled-in class name (for MSL drill-in tabs); falls back to
/// the open document's detected model name.
fn active_class_for_doc(world: &mut World, doc_id: lunco_doc::DocumentId) -> Option<String> {
    if let Some(m) = world.get_resource::<DrilledInClassNames>() {
        if let Some(c) = m.get(doc_id) {
            return Some(c.to_string());
        }
    }
    world
        .get_resource::<WorkbenchState>()
        .and_then(|ws| ws.open_model.as_ref())
        .and_then(|o| o.detected_name.clone())
}