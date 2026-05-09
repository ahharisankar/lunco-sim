//! `UsdViewportPanel` — 3D scene of the active USD document, rendered
//! to an offscreen [`Image`] and surfaced in egui as a regular
//! [`bevy_egui::egui::Image`].
//!
//! Mirrors the canvas pattern in spirit: one workbench panel, content
//! follows the active document. Different in execution because the
//! body is a real Bevy 3D render — we hand the egui panel a
//! `TextureId` whose underlying `Image` is what a [`Camera3d`] just
//! drew into.
//!
//! ## Why a singleton viewport (for now)
//!
//! Phase 6 ships **one shared viewport** that swaps which document
//! it shows when the user clicks a stage in the browser. That's what
//! the user-visible flow needs (one 3D scene at a time, just like
//! Omniverse's stage view) and avoids the per-document camera /
//! image / `BigSpace` triplication a multi-viewport implementation
//! would require. Multi-document side-by-side viewports are a
//! follow-up — the singleton seam is where they'll plug in.
//!
//! ## Pipeline
//!
//! ```text
//! UsdDocument source text
//!         │
//!         ▼  (on DocumentOpened / DocumentChanged for an active doc)
//! openusd::usda::parser  →  TextReader  →  UsdStageAsset
//!         │
//!         ▼  (Assets<UsdStageAsset>::get_mut, in-place swap)
//! Handle<UsdStageAsset>
//!         │
//!         ▼  (UsdPrimPath { stage_handle, path: "/" } on scene_root)
//! sync_usd_visuals  →  child entities with meshes / transforms
//!         │
//!         ▼  (Camera3d targets a render-to-texture Image)
//! Image  →  EguiUserTextures  →  egui::TextureId
//!         │
//!         ▼  (panel render)
//! UsdViewportPanel  ─────────  egui::Image in the dock
//! ```
//!
//! ## Lifecycle (observers)
//!
//! - [`DocumentOpened`] for our kind
//!   → bootstrap render scaffolding on first open, set this doc as
//!   the active viewport target, parse + install asset, mount on
//!   `scene_root`.
//! - [`lunco_doc_bevy::DocumentChanged`] for the
//!   active doc → re-parse, **mutate the asset in-place** so the
//!   `Handle<UsdStageAsset>` stays valid, despawn synced children,
//!   clear the `UsdVisualSynced` marker on `scene_root` so
//!   `sync_usd_visuals` re-runs.
//! - [`DocumentClosed`] → if it was
//!   the active doc, drop the asset and clear `scene_root`'s
//!   `UsdPrimPath`. Render scaffolding (image, camera, BigSpace) is
//!   kept warm so the next open doesn't pay the bootstrap cost.
//!
//! ## What this plugin does *not* do
//!
//! - Camera orbit / pan / zoom controls. Camera transform is fixed
//!   today; orbit lands as a follow-up that reads egui pointer
//!   events.
//! - Multiple simultaneous viewports / split views.
//! - USD composition (`UsdComposer::flatten`). Sublayers /
//!   references resolve only when the canonical asset loader is
//!   used (i.e. drag-drop / `asset_server.load`); workbench-driven
//!   docs walk only the root layer until the composer is wired into
//!   the in-place rebuild path.

use bevy::prelude::*;
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::image::Image;
use bevy::asset::RenderAssetUsages;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy_egui::egui;
use bevy_egui::{EguiTextureHandle, EguiUserTextures};
use lunco_doc::DocumentId;
use lunco_doc_bevy::{DocumentChanged, DocumentClosed, DocumentOpened};
use lunco_usd_bevy::{UsdPrimPath, UsdStageAsset, UsdVisualSynced};
use lunco_core::{Command, on_command, register_commands};
use lunco_workbench::{Panel, PanelId, PanelSlot, WorkbenchAppExt};
use openusd::usda::TextReader;

use crate::registry::UsdDocumentRegistry;

/// Stable id of the workbench tab the viewport renders into.
pub const USD_VIEWPORT_PANEL_ID: PanelId = PanelId("usd::viewport");

/// Image dimensions for the offscreen render target. Generous enough
/// that the panel looks crisp at typical IDE side-dock widths;
/// scaling-on-resize is a follow-up (would require recreating the
/// `Image`, the `EguiUserTextures` registration, and the camera
/// target each time).
const VIEWPORT_WIDTH: u32 = 1280;
const VIEWPORT_HEIGHT: u32 = 800;

/// Plugin that wires the viewport pipeline. Must be added together
/// with `DefaultPlugins` (or any plugin set that ships
/// `Assets<Image>` + the rendering schedule) — gated checks make the
/// observers no-op when those resources are absent so headless tests
/// still link cleanly.
pub struct UsdViewportPlugin;

impl Plugin for UsdViewportPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<UsdViewportState>();
        app.register_panel(UsdViewportPanel);
        app.add_observer(on_doc_opened_for_viewport);
        app.add_observer(on_doc_changed_for_viewport);
        app.add_observer(on_doc_closed_for_viewport);
        register_all_commands(app);
    }
}

/// Singleton state for the shared viewport. Populated lazily on the
/// first USD document open so headless test apps that never load USD
/// pay no cost.
#[derive(Resource, Default)]
pub struct UsdViewportState {
    /// True once [`bootstrap`] has spawned camera + image + scene
    /// root. Subsequent opens skip rebuild.
    bootstrapped: bool,
    /// The render-target image fed to egui as a `TextureId`. `None`
    /// before bootstrap or in headless apps where `Assets<Image>` is
    /// absent.
    image: Option<Handle<Image>>,
    /// Egui texture id corresponding to [`Self::image`]. `None` when
    /// `EguiUserTextures` is absent (headless / non-render builds).
    tex_id: Option<egui::TextureId>,
    /// Root entity the camera + light + scene root parent under.
    /// One per app — viewports for different docs share it and just
    /// swap the `UsdPrimPath` handle.
    scene_root: Option<Entity>,
    /// The 3D camera rendering into [`Self::image`].
    camera: Option<Entity>,
    /// The document currently displayed, or `None` when no USD doc
    /// is open / active.
    active_doc: Option<DocumentId>,
    /// The asset handle we're driving. Same id across rebuilds — we
    /// mutate in place so spawned `UsdPrimPath` children keep
    /// resolving without re-spawning the whole tree.
    current_handle: Option<Handle<UsdStageAsset>>,
}

impl UsdViewportState {
    /// The active document being rendered, if any.
    pub fn active_doc(&self) -> Option<DocumentId> {
        self.active_doc
    }
}

// ─────────────────────────────────────────────────────────────────────
// Bootstrap
// ─────────────────────────────────────────────────────────────────────

/// First-time setup of the render scaffolding. Idempotent: returns
/// early when `state.bootstrapped` is already true. Skips silently
/// when running in a context without `Assets<Image>` /
/// `EguiUserTextures` (headless tests, server bins) — the lifecycle
/// observers gracefully no-op afterwards.
fn bootstrap(world: &mut World) {
    if world.resource::<UsdViewportState>().bootstrapped {
        return;
    }
    // Headless guard — no `Assets<Image>` resource means rendering
    // hasn't been added to the app. Skip rather than panic.
    if !world.contains_resource::<Assets<Image>>() {
        return;
    }

    // Build the offscreen render target.
    let image_handle = {
        let image = make_target_image(VIEWPORT_WIDTH, VIEWPORT_HEIGHT);
        world.resource_mut::<Assets<Image>>().add(image)
    };

    // Register with egui so the panel can address it via TextureId.
    // The resource may be absent in non-render apps; in that case we
    // still keep the image handle (the camera will render but nothing
    // will display it) and skip the registration.
    let tex_id = world
        .get_resource_mut::<EguiUserTextures>()
        .map(|mut tex| tex.add_image(EguiTextureHandle::Strong(image_handle.clone())));

    // Camera looking down the -Z axis at the origin from a sensible
    // pose for "show me what I'm building." Sized for a single rover
    // (a few metres across) — Phase 7+ will make this orbit-controlled.
    let mut commands = world.commands();
    let camera = commands
        .spawn((
            Camera3d::default(),
            Camera {
                clear_color: ClearColorConfig::Custom(Color::srgb(0.10, 0.10, 0.12)),
                ..default()
            },
            // In Bevy 0.18 `RenderTarget` is a separate required
            // component on the camera entity rather than a field of
            // `Camera`. Spawn-bundled here so the camera renders
            // straight to our offscreen image.
            RenderTarget::Image(ImageRenderTarget::from(image_handle.clone())),
            Transform::from_xyz(4.0, 3.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
            Name::new("UsdViewportCamera"),
        ))
        .id();

    // A directional light so meshes aren't black. Shared with the
    // scene_root parent so it lights everything the camera sees.
    commands.spawn((
        DirectionalLight {
            illuminance: 8_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_xyz(5.0, 10.0, 5.0).looking_at(Vec3::ZERO, Vec3::Y),
        Name::new("UsdViewportSun"),
    ));

    // Scene root — receives `UsdPrimPath { ..., path: "/" }` once a
    // document is active. Until then it's a bare empty Transform.
    let scene_root = commands
        .spawn((
            Transform::default(),
            Visibility::default(),
            Name::new("UsdViewportSceneRoot"),
        ))
        .id();

    world.flush();

    let mut state = world.resource_mut::<UsdViewportState>();
    state.bootstrapped = true;
    state.image = Some(image_handle);
    state.tex_id = tex_id;
    state.scene_root = Some(scene_root);
    state.camera = Some(camera);
}

/// Construct a render-target image with sensible defaults
/// (Bgra8UnormSrgb, RENDER_ATTACHMENT). Wrapped so the bootstrap
/// reads cleanly.
fn make_target_image(width: u32, height: u32) -> Image {
    // `Image::new_target_texture` does the right thing for us in
    // 0.18 (sets all three usage flags), but it picks default
    // sample_count etc. We want a simple linear-RGBA target — egui
    // displays sRGB so Bgra8UnormSrgb keeps colours right without
    // an extra conversion pass.
    // (Extent3d / TextureDimension are referenced through
    // new_target_texture so we don't import them as dead code.)
    let _ = (Extent3d::default(), TextureDimension::D2);
    Image::new_target_texture(
        width,
        height,
        TextureFormat::Bgra8UnormSrgb,
        None,
    )
    .with_data_filled() // ensure RenderAssetUsages includes RENDER_WORLD
}

trait ImageExt {
    fn with_data_filled(self) -> Self;
}

impl ImageExt for Image {
    fn with_data_filled(mut self) -> Self {
        // `new_target_texture` already fills with zeros and uses
        // RenderAssetUsages::default(). This shim documents the
        // intent and gives us a hook to flip flags later (e.g. drop
        // MAIN_WORLD if we ever fully migrate ownership to the
        // render world). No-op today.
        self.asset_usage = RenderAssetUsages::default();
        // The default usage flags from `new_target_texture` already
        // include RENDER_ATTACHMENT — assert we didn't accidentally
        // strip them.
        debug_assert!(self
            .texture_descriptor
            .usage
            .contains(TextureUsages::RENDER_ATTACHMENT));
        self
    }
}

// ─────────────────────────────────────────────────────────────────────
// SetActiveUsdViewport — typed command for "show this stage"
// ─────────────────────────────────────────────────────────────────────

/// Make `doc` the active stage in the singleton viewport. Browser
/// row clicks fire this; HTTP API / MCP / scripts can fire it
/// directly. Idempotent — calling with the already-active doc is a
/// no-op.
#[Command(default)]
pub struct SetActiveUsdViewport {
    /// The USD document to surface in the viewport.
    pub doc: DocumentId,
}

#[on_command(SetActiveUsdViewport)]
fn on_set_active_usd_viewport(
    trigger: On<SetActiveUsdViewport>,
    mut commands: Commands,
) {
    let doc = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        if !world.resource::<UsdDocumentRegistry>().contains(doc) {
            return;
        }
        if world.resource::<UsdViewportState>().active_doc == Some(doc) {
            return;
        }
        bootstrap(world);
        // Detach old before installing new so the asset reference
        // count drops cleanly.
        if let Some(scene_root) = world.resource::<UsdViewportState>().scene_root {
            if let Ok(mut entity) = world.get_entity_mut(scene_root) {
                entity.remove::<UsdPrimPath>();
                entity.remove::<UsdVisualSynced>();
                entity.despawn_related::<Children>();
            }
        }
        install_active_doc(world, doc);
    });
}

register_commands!(on_set_active_usd_viewport,);

// ─────────────────────────────────────────────────────────────────────
// Document lifecycle observers
// ─────────────────────────────────────────────────────────────────────

fn on_doc_opened_for_viewport(
    trigger: On<DocumentOpened>,
    mut commands: Commands,
) {
    let doc = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        // Gate on USD ownership so Modelica / SysML opens skip.
        if !world.resource::<UsdDocumentRegistry>().contains(doc) {
            return;
        }
        bootstrap(world);
        // Make this the active doc if nothing else is showing. Phase
        // 6 has one shared viewport — later opens stay queued; the
        // user can switch by clicking a row in the browser (which
        // dispatches a future SetActiveUsdViewport command).
        if world.resource::<UsdViewportState>().active_doc.is_none() {
            install_active_doc(world, doc);
        }
    });
}

fn on_doc_changed_for_viewport(
    trigger: On<DocumentChanged>,
    mut commands: Commands,
) {
    let doc = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let state = world.resource::<UsdViewportState>();
        if state.active_doc != Some(doc) {
            return;
        }
        rebuild_active_asset(world);
    });
}

fn on_doc_closed_for_viewport(
    trigger: On<DocumentClosed>,
    mut commands: Commands,
) {
    let doc = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let mut state = world.resource_mut::<UsdViewportState>();
        if state.active_doc != Some(doc) {
            return;
        }
        state.active_doc = None;
        state.current_handle = None;
        // Detach scene_root from the asset; sync_usd_visuals will
        // skip with no UsdPrimPath.
        let scene_root = state.scene_root;
        drop(state);
        if let Some(root) = scene_root {
            if let Ok(mut entity) = world.get_entity_mut(root) {
                entity.remove::<UsdPrimPath>();
                entity.remove::<UsdVisualSynced>();
                entity.despawn_related::<Children>();
            }
        }
    });
}

// ─────────────────────────────────────────────────────────────────────
// Asset install / rebuild
// ─────────────────────────────────────────────────────────────────────

/// Install `doc` as the viewport's active stage. Parses the source,
/// adds the asset, attaches `UsdPrimPath` to `scene_root`. No-op when
/// rendering scaffolding hasn't been bootstrapped (headless apps).
fn install_active_doc(world: &mut World, doc: DocumentId) {
    let Some(scene_root) = world.resource::<UsdViewportState>().scene_root else {
        return;
    };
    let Some(source) = world
        .resource::<UsdDocumentRegistry>()
        .host(doc)
        .map(|h| h.document().source().to_string())
    else {
        return;
    };
    let Some(reader) = parse_reader(&source) else {
        bevy::log::warn!("[UsdViewport] could not parse {} for viewport", doc);
        return;
    };
    let asset = UsdStageAsset {
        reader: std::sync::Arc::new(reader),
    };
    let handle = world
        .resource_mut::<Assets<UsdStageAsset>>()
        .add(asset);
    if let Ok(mut entity) = world.get_entity_mut(scene_root) {
        entity.insert(UsdPrimPath {
            stage_handle: handle.clone(),
            path: "/".to_string(),
        });
        entity.remove::<UsdVisualSynced>();
        entity.despawn_related::<Children>();
    }
    let mut state = world.resource_mut::<UsdViewportState>();
    state.active_doc = Some(doc);
    state.current_handle = Some(handle);
}

/// Rebuild the active stage from its document's current source,
/// mutating the existing asset in place so the `Handle` stays valid.
/// Called from the `DocumentChanged` observer.
fn rebuild_active_asset(world: &mut World) {
    let (handle, doc) = {
        let state = world.resource::<UsdViewportState>();
        match (state.current_handle.clone(), state.active_doc) {
            (Some(h), Some(d)) => (h, d),
            _ => return,
        }
    };
    let Some(source) = world
        .resource::<UsdDocumentRegistry>()
        .host(doc)
        .map(|h| h.document().source().to_string())
    else {
        return;
    };
    let Some(reader) = parse_reader(&source) else {
        bevy::log::warn!("[UsdViewport] re-parse failed for {}", doc);
        return;
    };
    if let Some(asset) = world
        .resource_mut::<Assets<UsdStageAsset>>()
        .get_mut(&handle)
    {
        asset.reader = std::sync::Arc::new(reader);
    }
    // Invalidate sync_usd_visuals output so the system re-walks the
    // (now-updated) asset and respawns the prim entity tree.
    if let Some(scene_root) = world.resource::<UsdViewportState>().scene_root {
        if let Ok(mut entity) = world.get_entity_mut(scene_root) {
            entity.remove::<UsdVisualSynced>();
            entity.despawn_related::<Children>();
        }
    }
}

/// Parse a `.usda` source string into a `TextReader`. Returns `None`
/// on parse error; callers log and bail.
///
/// Composition (`UsdComposer::flatten`) is intentionally **not**
/// applied here — workbench-driven docs walk only their root layer
/// until the composer is wired into the in-place rebuild path. The
/// canonical asset loader (used by drag-drop / `asset_server.load`)
/// keeps full composition behaviour.
fn parse_reader(source: &str) -> Option<TextReader> {
    let mut parser = openusd::usda::parser::Parser::new(source);
    match parser.parse() {
        Ok(data) => Some(TextReader::from_data(data)),
        Err(_) => None,
    }
}

// ─────────────────────────────────────────────────────────────────────
// UsdViewportPanel
// ─────────────────────────────────────────────────────────────────────

/// Singleton workbench panel that displays the active USD viewport.
pub struct UsdViewportPanel;

impl Panel for UsdViewportPanel {
    fn id(&self) -> PanelId {
        USD_VIEWPORT_PANEL_ID
    }

    fn title(&self) -> String {
        "USD Viewport".to_string()
    }

    fn default_slot(&self) -> PanelSlot {
        PanelSlot::Center
    }

    fn closable(&self) -> bool {
        false
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        let (tex_id, name) = {
            let state = world.resource::<UsdViewportState>();
            let tex_id = state.tex_id;
            let name = state
                .active_doc
                .and_then(|d| {
                    world
                        .get_resource::<UsdDocumentRegistry>()
                        .and_then(|r| r.host(d))
                        .map(|h| h.document().origin().display_name())
                })
                .unwrap_or_else(|| "(no stage)".to_string());
            (tex_id, name)
        };

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&name).strong());
        });
        ui.separator();

        let Some(tex_id) = tex_id else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Open a USD stage to see it here. \
                         Render scaffolding boots on first open.",
                    )
                    .weak()
                    .italics(),
                );
            });
            return;
        };

        // Fill the panel rect, preserving the image's aspect ratio.
        let avail = ui.available_size();
        let aspect = VIEWPORT_WIDTH as f32 / VIEWPORT_HEIGHT as f32;
        let mut size = avail;
        if size.x / size.y > aspect {
            size.x = size.y * aspect;
        } else {
            size.y = size.x / aspect;
        }
        ui.add(egui::Image::new(egui::load::SizedTexture::new(tex_id, size)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::UsdCommandsPlugin;
    use lunco_doc::DocumentOrigin;

    /// Without any rendering plugins (`Assets<Image>` absent) the
    /// observers gracefully no-op — the state stays
    /// non-bootstrapped, no panic.
    #[test]
    fn lifecycle_is_headless_safe() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(UsdCommandsPlugin);
        app.add_plugins(UsdViewportPlugin);
        app.update();

        let _doc = {
            let mut reg = app.world_mut().resource_mut::<UsdDocumentRegistry>();
            reg.allocate(
                "#usda 1.0\n".into(),
                DocumentOrigin::writable_file("/tmp/x.usda"),
            )
        };
        // Drain pending events twice so the DocumentOpened trigger
        // fires and our observer runs.
        app.update();
        app.update();

        let state = app.world().resource::<UsdViewportState>();
        // No render scaffolding in MinimalPlugins → bootstrap bailed.
        assert!(!state.bootstrapped);
        assert!(state.image.is_none());
        assert!(state.tex_id.is_none());
        // active_doc gates on bootstrap so we don't half-attach.
        // The current code sets active_doc *after* bootstrap (which
        // bailed), so it should still be None.
        assert!(state.active_doc.is_none());
    }
}
