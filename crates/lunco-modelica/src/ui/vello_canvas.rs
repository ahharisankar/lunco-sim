//! `vello_canvas` — Phase 1 diagram rendering through bevy_vello.
//!
//! Per open document tab we keep an offscreen render target (an
//! `Image` plus its egui texture id) and a `Camera2d` + `VelloScene2d`
//! pair that draws into that image. Each frame a system converts
//! the active tab's `lunco_canvas::Scene` into vello paths in world
//! coordinates; the diagram panel shows the resulting texture via
//! `egui::Image`. Egui keeps owning all interaction (selection,
//! drag, tools); vello is "just" a renderer.
//!
//! This is the Phase-1 milestone from
//! `docs/architecture/canvas-vello.md` (TBD). The egui-based custom
//! draw path stays in place during the migration so the workbench
//! never breaks; we'll retire it in Phase 3.

use bevy::asset::RenderAssetUsages;
use bevy::camera::RenderTarget;
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDimension, TextureFormat, TextureUsages,
};
use bevy_egui::{egui, EguiContexts, EguiTextureHandle, EguiUserTextures};
use bevy_vello::prelude::*;
use bevy_vello::vello::{
    kurbo::{Affine, BezPath, Rect, RoundedRect, Stroke},
    peniko::{Color, Fill},
};
use lunco_doc::DocumentId;

use crate::ui::panels::canvas_diagram::CanvasDiagramState;

/// Default render-target dimensions when a tab first opens. Resized
/// later if the panel grew (Phase 1.5).
const DEFAULT_TEX_W: u32 = 1280;
const DEFAULT_TEX_H: u32 = 800;

/// Per-document vello render-target bookkeeping. One entry per
/// currently open tab. Allocated on first sight of a `CanvasDiagramState`
/// for that doc, freed when the tab closes.
#[derive(Resource, Default)]
pub struct VelloCanvasTargets {
    by_doc: bevy::platform::collections::HashMap<DocumentId, TabTarget>,
}

struct TabTarget {
    /// The image vello renders into.
    image: Handle<Image>,
    /// Cached egui-side handle for `egui::Image::from_texture`.
    /// Captured at creation time — touching `EguiUserTextures`
    /// per-frame conflicts with bevy_egui's own borrow.
    texture_id: egui::TextureId,
    /// The Camera entity carrying `VelloView` + `RenderTarget::Image`.
    camera: Entity,
    /// The `VelloScene2d` entity we re-fill each frame.
    scene: Entity,
    /// Last allocated texture size. Future resize pass compares
    /// against the panel's current rect.
    #[allow(dead_code)]
    size: (u32, u32),
}

impl VelloCanvasTargets {
    /// Resolve the egui texture id for `doc`, if a target exists.
    /// The diagram panel calls this each frame to embed the texture.
    pub fn texture_id(&self, doc: DocumentId) -> Option<egui::TextureId> {
        self.by_doc.get(&doc).map(|t| t.texture_id)
    }
}

/// Plugin entry point — register the resource, add the per-frame
/// systems. Slot in `app.add_plugins(VelloCanvasPlugin)` once
/// `VelloPlugin` is already installed.
pub struct VelloCanvasPlugin;

impl Plugin for VelloCanvasPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VelloCanvasTargets>()
            .add_systems(
                Update,
                (
                    ensure_targets_for_open_tabs,
                    draw_diagram_into_vello_scene,
                )
                    .chain(),
            );
        // Phase-1 floating debug window retired — `CanvasDiagramPanel`
        // now composites the texture as the canvas backdrop directly.
    }
}

/// Allocate a render target (image + camera + scene) for any
/// `DocumentId` that has a `CanvasDiagramState` but no entry in
/// `VelloCanvasTargets` yet. Symmetric "free on close" pass is a
/// follow-up — the tab-close path doesn't notify this module yet.
fn ensure_targets_for_open_tabs(
    mut commands: Commands,
    mut targets: ResMut<VelloCanvasTargets>,
    mut images: ResMut<Assets<Image>>,
    mut egui_user_textures: ResMut<EguiUserTextures>,
    canvas_state: Option<Res<CanvasDiagramState>>,
) {
    let Some(canvas_state) = canvas_state else { return };
    for doc in canvas_state.iter_doc_ids() {
        if targets.by_doc.contains_key(&doc) {
            continue;
        }
        let (image, texture_id) = allocate_target(
            DEFAULT_TEX_W,
            DEFAULT_TEX_H,
            &mut images,
            &mut egui_user_textures,
        );
        let camera = commands
            .spawn((
                Camera2d,
                Camera::default(),
                RenderTarget::Image(image.clone().into()),
                VelloView,
                VelloCanvasFor(doc),
            ))
            .id();
        let scene = commands
            .spawn((VelloScene2d::default(), VelloCanvasFor(doc)))
            .id();
        targets.by_doc.insert(
            doc,
            TabTarget {
                image,
                texture_id,
                camera,
                scene,
                size: (DEFAULT_TEX_W, DEFAULT_TEX_H),
            },
        );
        info!(
            "[VelloCanvas] allocated render target for doc {:?} ({}×{})",
            doc, DEFAULT_TEX_W, DEFAULT_TEX_H
        );
    }
    // Suppress unused-warning churn while the field is still settling.
    let _ = (commands, targets);
}

/// Marker so we can later despawn the camera + scene tied to a
/// closed tab. Phase 1.5 wiring; not consulted yet.
#[derive(Component)]
struct VelloCanvasFor(DocumentId);

fn allocate_target(
    width: u32,
    height: u32,
    images: &mut Assets<Image>,
    egui_user_textures: &mut EguiUserTextures,
) -> (Handle<Image>, egui::TextureId) {
    let size = Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let mut image = Image::new_fill(
        size,
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );
    image.texture_descriptor.usage = TextureUsages::TEXTURE_BINDING
        | TextureUsages::COPY_DST
        | TextureUsages::RENDER_ATTACHMENT;
    let handle = images.add(image);
    let texture_id = egui_user_textures
        .add_image(EguiTextureHandle::Strong(handle.clone()));
    (handle, texture_id)
}

/// Per-frame: walk every open tab's canvas scene and emit vello
/// paths into the matching `VelloScene2d`. First cut paints each
/// node as a filled rounded rectangle in canvas world coords —
/// proves the pipeline; primitive-fidelity rendering (icons,
/// edges, labels, ports) lands in Phase 1.5+.
fn draw_diagram_into_vello_scene(
    targets: Res<VelloCanvasTargets>,
    canvas_state: Option<Res<CanvasDiagramState>>,
    mut scenes: Query<&mut VelloScene2d>,
) {
    let Some(canvas_state) = canvas_state else { return };
    for (doc, target) in targets.by_doc.iter() {
        let Ok(mut scene) = scenes.get_mut(target.scene) else { continue };
        scene.reset();
        // `get` (not `get_for_doc`) falls back to the unbound
        // `CanvasDiagramState.fallback` slot when `per_doc[doc]` is
        // absent. The existing canvas projector has a known race
        // where it captures `active_document = None` at task spawn
        // and lands the projected scene in `fallback` instead of the
        // intended doc. Using `get` here means vello still renders
        // *some* scene during that race; once the panel calls
        // `get_mut(Some(doc))` later, fallback drains into per_doc
        // and the texture stays in sync.
        let doc_state = canvas_state.get(Some(*doc));
        // Canvas-bg fill so the texture isn't transparent — gives
        // the diagram a defined backdrop independent of egui's
        // surrounding panel colour. Drawn in screen space (no
        // scale) sized to the full texture extent.
        scene.fill(
            Fill::NonZero,
            Affine::default(),
            Color::new([0.10, 0.10, 0.12, 1.0]),
            None,
            &Rect::new(
                -(target.size.0 as f64) / 2.0,
                -(target.size.1 as f64) / 2.0,
                target.size.0 as f64 / 2.0,
                target.size.1 as f64 / 2.0,
            ),
        );
        // DEBUG: alignment marker — a magenta cross at the texture
        // centre, drawn in screen space so we know the camera /
        // target plumbing works. If the diagram nodes vanish but
        // this cross is visible, the bug is in node-coord mapping;
        // if the cross is also missing, the camera or render-target
        // is misconfigured.
        scene.fill(
            Fill::NonZero,
            Affine::default(),
            Color::new([1.0, 0.0, 1.0, 0.9]),
            None,
            &Rect::new(-40.0, -3.0, 40.0, 3.0),
        );
        scene.fill(
            Fill::NonZero,
            Affine::default(),
            Color::new([1.0, 0.0, 1.0, 0.9]),
            None,
            &Rect::new(-3.0, -40.0, 3.0, 40.0),
        );
        // DEBUG: log the node count + first rect once per change.
        let node_count = doc_state.canvas.scene.nodes().count();
        if let Some((_, n)) = doc_state.canvas.scene.nodes().next() {
            bevy::log::info!(
                "[VelloCanvas] {} nodes, first rect: ({:.1},{:.1})..({:.1},{:.1})",
                node_count, n.rect.min.x, n.rect.min.y, n.rect.max.x, n.rect.max.y,
            );
        }

        // Single Affine for the whole world transform: ties the
        // vello render to the same Viewport (pan + zoom) the egui
        // canvas uses, so the texture aligns pixel-for-pixel with
        // the egui-drawn content composited on top of it. The
        // Camera2d sits at the origin; we translate the world so
        // the viewport's `pan` lands at the centre of the texture,
        // then scale by `zoom`. Y stays unflipped because the
        // canvas world model already runs +Y down (egui screen
        // convention) — the Modelica +Y-up flip happened earlier
        // in the projection.
        let viewport = &doc_state.canvas.viewport;
        let zoom = viewport.zoom as f64;
        let center_x = viewport.center.x as f64;
        let center_y = viewport.center.y as f64;
        // egui canvas screen↔world maps:  screen = mid + (world - center) * zoom
        // (see lunco_canvas::Viewport::world_to_screen). We mirror
        // that here using vello's bottom-up Affine convention so the
        // vello-drawn texture aligns pixel-for-pixel with the egui
        // canvas. `mid` is the texture centre.
        let mid_x = (target.size.0 as f64) / 2.0;
        let mid_y = (target.size.1 as f64) / 2.0;
        // Vello's Camera2d puts the texture origin at its centre,
        // so the screen coords we want are already centred on the
        // image. The Affine therefore matches the canvas's
        // world_to_screen formula minus the `mid` (which the
        // camera handles): `screen' = (world - center) * zoom`.
        let xform = Affine::scale(zoom)
            * Affine::translate((-center_x, -center_y));
        let _ = (mid_x, mid_y); // mid is implicit from camera centring

        // Edges first — drawn UNDER the nodes so port circles sit on
        // top of wire ends, matching OMEdit.
        let canvas_scene = &doc_state.canvas.scene;
        for (_eid, edge) in canvas_scene.edges() {
            let Some(from_node) = canvas_scene.node(edge.from.node) else { continue };
            let Some(to_node) = canvas_scene.node(edge.to.node) else { continue };
            let Some(from_port) = from_node
                .ports
                .iter()
                .find(|p| p.id == edge.from.port)
            else {
                continue;
            };
            let Some(to_port) = to_node.ports.iter().find(|p| p.id == edge.to.port) else {
                continue;
            };
            let a = (
                from_node.rect.min.x as f64 + from_port.local_offset.x as f64,
                from_node.rect.min.y as f64 + from_port.local_offset.y as f64,
            );
            let b = (
                to_node.rect.min.x as f64 + to_port.local_offset.x as f64,
                to_node.rect.min.y as f64 + to_port.local_offset.y as f64,
            );
            let mut path = BezPath::new();
            path.move_to(a);
            path.line_to(b);
            scene.stroke(
                &Stroke::new(0.4),
                xform,
                // Generic wire color — phase 2 will read connector
                // type and route through the real palette.
                Color::new([0.55, 0.65, 0.85, 1.0]),
                None,
                &path,
            );
        }

        // Each node → filled rounded rect outlined in soft grey.
        for (_id, node) in canvas_scene.nodes() {
            let r = &node.rect;
            let rect = RoundedRect::new(
                r.min.x as f64,
                r.min.y as f64,
                r.max.x as f64,
                r.max.y as f64,
                1.5,
            );
            scene.fill(
                Fill::NonZero,
                xform,
                Color::new([0.95, 0.95, 0.96, 1.0]),
                None,
                &rect,
            );
            scene.stroke(
                &Stroke::new(0.3),
                xform,
                Color::new([0.30, 0.32, 0.36, 1.0]),
                None,
                &rect,
            );
        }
    }
    // Suppress unused-warning churn while the migration is in flight.
    let _ = scenes;
}

/// Embed the active tab's vello render target inside an egui Ui at
/// `rect`. Called from `CanvasDiagramPanel::render` once Phase 1's
/// switch is flipped on. Returns the `egui::Response` so callers can
/// chain interaction logic (clicks, hover) on top.
pub fn show_in_ui(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    texture_id: egui::TextureId,
) -> egui::Response {
    let painter = ui.painter_at(rect);
    painter.image(
        texture_id,
        rect,
        egui::Rect::from_min_max(
            egui::pos2(0.0, 0.0),
            egui::pos2(1.0, 1.0),
        ),
        egui::Color32::WHITE,
    );
    ui.allocate_rect(rect, egui::Sense::click_and_drag())
}

/// Diagnostic floating window — temporarily shows every tab's vello
/// texture in a single egui window so we can verify Phase 1 is
/// rendering before we wire the switch into the actual diagram
/// panel. Remove once the panel-side switch lands.
pub fn debug_window(
    mut contexts: EguiContexts,
    targets: Res<VelloCanvasTargets>,
) {
    let Ok(ctx) = contexts.ctx_mut() else { return };
    egui::Window::new("Vello (Phase 1 debug)")
        .resizable(true)
        .default_size([520.0, 400.0])
        .show(ctx, |ui: &mut egui::Ui| {
            if targets.by_doc.is_empty() {
                ui.label("No diagram tabs open yet.");
                return;
            }
            for (doc, target) in targets.by_doc.iter() {
                ui.label(format!("doc {:?}", doc));
                ui.image(egui::load::SizedTexture::new(
                    target.texture_id,
                    egui::vec2(480.0, 320.0),
                ));
                ui.separator();
            }
        });
}
