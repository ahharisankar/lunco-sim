//! API-driven focus + connection pulse layers.
//!
//! Edges connecting newly-API-added components flash for a short
//! window so users notice them; recently-API-focused entities glow
//! with an outer ring. Both effects are built from per-entry
//! `PulseEntry<T>` records driven by background tickers
//! (`drive_pending_api_focus`, `drive_pending_api_connections`).

use std::collections::HashMap;

use bevy::prelude::*;
use bevy_egui::egui;

use super::CanvasDiagramState;

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
pub(super) struct EdgePulseLayer {
    pub(super) data: EdgePulseHandle,
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
pub(super) fn port_world_pos(
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
        let anim_ms = entry.animation_ms;
        // Fan out to *every* tab viewing this doc, not just the
        // first. Edge ids are scene-local — each tab projects its own
        // scene with its own ids — so we re-find the edge per tab
        // using the same component+port predicate and push a pulse
        // into each tab that contains a match. Fixes the valve-glow
        // regression where split-view tabs only saw pulses on the
        // first tab.
        let mut any_pulsed = false;
        for (_, d, ds) in state.iter_mut() {
            if d != entry.doc {
                continue;
            }
            // Match by node `origin` (component name) + port id
            // (port name). The canvas projection puts the port's
            // name in `Port.id`'s string form via SmolStr; matching
            // by id works because the projector keys ports by
            // simple name.
            let hit_id = {
                let scene = &ds.canvas.scene;
                scene
                    .edges()
                    .find(|(_, e)| {
                        let from_node = scene.node(e.from.node);
                        let to_node = scene.node(e.to.node);
                        let from_match = from_node
                            .map(|n| {
                                n.origin.as_deref()
                                    == Some(entry.from_component.as_str())
                                    && n.ports.iter().any(|p| {
                                        p.id == e.from.port
                                            && p.id.as_str()
                                                == entry.from_port.as_str()
                                    })
                            })
                            .unwrap_or(false);
                        let to_match = to_node
                            .map(|n| {
                                n.origin.as_deref()
                                    == Some(entry.to_component.as_str())
                                    && n.ports.iter().any(|p| {
                                        p.id == e.to.port
                                            && p.id.as_str()
                                                == entry.to_port.as_str()
                                    })
                            })
                            .unwrap_or(false);
                        from_match && to_match
                    })
                    .map(|(eid, _)| *eid)
            };
            if let Some(edge_id) = hit_id {
                if anim_ms > 0 {
                    if let Ok(mut guard) = ds.edge_pulse_handle.write() {
                        guard.push(PulseEntry {
                            id: edge_id,
                            started: web_time::Instant::now(),
                            duration_ms: anim_ms,
                        });
                    }
                }
                any_pulsed = true;
            }
        }
        if !any_pulsed {
            still_pending.push(entry);
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
pub(super) struct PulseGlowLayer {
    pub(super) data: PulseHandle,
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
    //
    // We capture entry *names* per-doc rather than pre-resolving
    // `NodeId`s — node ids are scene-local, so a node id from the
    // first tab won't match the same logical node in a sibling
    // tab's scene. The fan-out below re-finds the node per tab.
    let mut matched: std::collections::HashMap<
        lunco_doc::DocumentId,
        Vec<(String /* name */, u32 /* animation_ms */)>,
    > = std::collections::HashMap::new();
    let mut any_still_unmatched_within_timeout = false;
    for entry in queue.0.iter() {
        // Use first-tab projection to test "does this name resolve
        // *somewhere* yet?". The actual per-tab node id is
        // re-resolved in the fan-out.
        let docstate = state.get(Some(entry.doc));
        let resolved = docstate
            .canvas
            .scene
            .nodes()
            .any(|(_, n)| n.origin.as_deref() == Some(entry.name.as_str()));
        if resolved {
            matched
                .entry(entry.doc)
                .or_default()
                .push((entry.name.clone(), entry.animation_ms));
        } else if now.duration_since(entry.queued_at) <= FOCUS_TIMEOUT {
            any_still_unmatched_within_timeout = true;
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
    // Fan out across every tab viewing each doc. Each tab gets its
    // own pulse entries (resolved by `entry.name` against the tab's
    // scene) and its own `pending_fit` flag — fitting in tab A must
    // not move tab B's camera. Fixes the focus regression where
    // split-view tabs only animated the first tab.
    for (_, d, ds) in state.iter_mut() {
        let Some(entries) = matched.get(&d) else { continue };

        if let Ok(mut guard) = ds.pulse_handle.write() {
            for (i, (name, anim_ms)) in entries.iter().enumerate() {
                if *anim_ms == 0 {
                    continue;
                }
                let Some((node_id, _)) = ds
                    .canvas
                    .scene
                    .nodes()
                    .find(|(_, n)| n.origin.as_deref() == Some(name.as_str()))
                else {
                    continue;
                };
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
        // actual `response.rect` is in scope, so the fit math uses
        // the real widget size — not the 1280×800 approximation
        // we'd have to guess at here. It calls
        // `viewport.set_target`, which animates via the viewport's
        // built-in exponential ease.
        ds.pending_fit = true;
    }
}