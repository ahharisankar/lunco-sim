//! `CanvasDiagramPanel` — top-level panel implementation.
//!
//! Owns the long-running render system, event drivers, and the per-
//! frame canvas refresh. Splits the rest of the canvas-diagram
//! pipeline (projection, ops, overlays, menus, decorations, pulse,
//! palette) into sibling submodules and orchestrates them here.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_canvas::Scene;
use lunco_doc::Document;
use lunco_workbench::{Panel, PanelId, PanelSlot};

use crate::document::ModelicaOp;
use crate::ui::state::ModelicaDocumentRegistry;

use super::loads::{DrillInLoads, DuplicateLoads, drill_into_class};
use super::node::IconNodeData;
use super::ops::{self, apply_ops_public};
use super::projection::{
    ProjectionTask, project_scene, projection_relevant_source_hash, recover_edges_from_ast,
};
use super::theme::{
    CanvasThemeSnapshot, layer_theme_from, store_canvas_theme, store_modelica_icon_palette,
};
use super::{
    CANVAS_DIAGRAM_PANEL_ID, CanvasDiagramState, CanvasSnapSettings, ContextMenuTarget,
    DiagramProjectionLimits, ICON_W, PendingContextMenu, active_doc_from_world, decorations,
    menus, overlays, render_target,
};

// Per-event sibling-scene replay (`apply_event_to_sibling_scene`)
// removed. Sibling tabs viewing the same `(doc,
// drilled_class)` now re-derive their scene from the gen-bumped AST
// on the next render, via the projection cursor in `render_canvas`.
// This is correct-by-construction: every mutation flows through
// `host.apply` → `op_to_patch` → AST mutation + source rewrite →
// `DocumentChanged` event → projection invalidates on next gen
// observation. Per-event mirroring would now be a redundant write
// path that could drift from the AST.

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

        // Snapshot the rendering tab's identity so every
        // CanvasDiagramState lookup below keys *this* tab's entry.
        // Each tab gets its own viewport / selection / scene —
        // splits of the same model can pan, zoom, and select
        // independently.
        // `render_drilled` snapshot deleted — `render_tab_id`
        // alone scopes the lookup correctly. `ModelTabState` carries
        // the drilled class per-tab; nothing in this render path
        // needs the drilled name as a separate variable.
        let render_tab_id: Option<crate::ui::panels::model_view::TabId> = world
            .resource::<crate::ui::panels::model_view::TabRenderContext>()
            .tab_id;

        // Decide whether to rebuild the scene. Per-doc state means
        // "bound_doc" is implicit in the map key — a fresh entry has
        // `last_seen_gen == 0` so the first render after tab open
        // always re-projects.
        let project_now = {
            // Resolve target tab: prefer the per-render
            // [`TabRenderContext`] so a split sees its own tab; fall
            // back to the workspace-wide active doc for non-tab
            // render paths.
            let Some(doc_id) = active_doc_from_world(world)
            else {
                world
                    .resource_mut::<CanvasDiagramState>()
                    .get_mut(None)
                    .canvas
                    .scene = Scene::new();
                self.render_canvas(ui, world);
                return;
            };
            if world
                .resource::<lunco_workbench::WorkspaceResource>()
                .active_document
                .and_then(|d| world.resource::<ModelicaDocumentRegistry>().host(d))
                .is_none()
            {
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
            let docstate = match render_tab_id { Some(t) => state.get_for_tab(t), None => state.get(Some(doc_id)) };
            let first_render = !match render_tab_id { Some(t) => state.has_entry_for_tab(t), None => state.has_entry(doc_id) };
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
            // Prefer the rendering tab's own `drilled_class` over
            // the per-doc `DrilledInClassNames` map so split panes
            // can hold different drill targets on the same doc.
            let live_target = render_target(world)
                .filter(|(d, _)| *d == doc_id)
                .and_then(|(_, drilled)| drilled)
                .or_else(|| {
                    world
                        .get_resource::<crate::ui::panels::model_view::ModelTabs>()
                        .and_then(|t| t.drilled_class_for_doc(doc_id))
                });
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
                        let mut state =
                            world.resource_mut::<CanvasDiagramState>();
                        let docstate = match render_tab_id { Some(t) => state.get_mut_for_tab(t, doc_id), None => state.get_mut(Some(doc_id)) };
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
            // Set inside the spawn block when we actually spawn;
            // consumed *after* the `CanvasDiagramState` borrow ends
            // to attach a `BusyHandle` from `StatusBus`. Two-step
            // because the spawn site holds `&mut state` (line ~297)
            // and we can't borrow the bus through `world` while that
            // is live.
            let mut spawned_busy_label: Option<(String, Option<String>)> = None;
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
                // Engine is the canonical AST source.
                // If the engine hasn't parsed yet (first paint after
                // doc install), strict_ast() returns None — paint
                // the loading overlay and retry next tick. Mirrors
                // the host-not-found branch above.
                let Some(ast) = doc.strict_ast() else {
                    self.render_canvas(ui, world);
                    return;
                };
                (doc.source().to_string(), ast)
            };
            // `drive_engine_sync` (now AST-based, near-free per
            // upsert via `add_parsed_batch`) populates the engine on
            // the same Update tick the doc was installed. Projection
            // either runs after engine sync this tick, or one tick
            // later — either way the upsert is microseconds, not the
            // ~370ms reparse the old sync path here cost.
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
            // Reading the doc origin display name doesn't work here —
            // for installed docs it's the filesystem path, not the
            // `msl://…` URI. `None` for Untitled / user-authored
            // docs — builder picks the first non-package class as
            // before.
            // Prefer the rendering tab's own scope so split panes
            // with distinct drill targets project distinct scenes.
            let target_class_snapshot: Option<String> = render_target(world)
                .filter(|(d, _)| *d == doc_id)
                .and_then(|(_, drilled)| drilled)
                .or_else(|| {
                    world
                        .get_resource::<crate::ui::panels::model_view::ModelTabs>()
                        .and_then(|t| t.drilled_class_for_doc(doc_id))
                });
            // Snapshot the auto-layout grid so the bg task can fall
            // back to configurable spacing for components without a
            // `Placement` annotation.
            let layout_snapshot = world
                .get_resource::<crate::ui::panels::canvas_projection::DiagramAutoLayoutSettings>()
                .cloned()
                .unwrap_or_default();
            let mut state = world.resource_mut::<CanvasDiagramState>();
            let docstate = match render_tab_id { Some(t) => state.get_mut_for_tab(t, doc_id), None => state.get_mut(Some(doc_id)) };
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
            let diag = decorations::diagram_annotation_for_target(
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
                // Cooperative-cancel the old task before dropping it.
                // Bevy's AsyncCompute on wasm runs cooperatively on
                // the main thread — a not-cancelled task keeps running
                // through every `should_stop()` check it makes (which
                // returns false) all the way to completion, burning
                // main-thread time the active tab needs. Flipping the
                // `cancel` AtomicBool means the next `should_stop()`
                // check inside the task short-circuits and returns an
                // empty Scene.
                if let Some(t) = docstate.projection_task.as_ref() {
                    t.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
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
                        // Yield to the runtime BEFORE doing any heavy
                        // work. On wasm `AsyncComputeTaskPool` runs
                        // cooperatively on the main thread under
                        // `wasm_bindgen_futures`; without explicit
                        // `await` points the spawned future runs to
                        // completion in one synchronous burst,
                        // freezing egui for ~800 ms on a fresh
                        // AnnotatedRocketStage projection. Each
                        // `yield_now().await` returns control to the
                        // microtask queue so the next egui frame can
                        // paint before we resume. Native: noop, the
                        // thread-pool worker isn't on the UI thread.
                        futures_lite::future::yield_now().await;
                        if should_stop() {
                            return Scene::new();
                        }
                        let t0 = web_time::Instant::now();
                        // Hold an Arc clone so `recover_edges_from_ast`
                        // can read the same AST after the import call
                        // takes ownership. Arc clone = pointer bump,
                        // not a tree clone.
                        let ast_for_recover = std::sync::Arc::clone(&ast_arc);
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
                        futures_lite::future::yield_now().await;
                        if should_stop() {
                            return Scene::new();
                        }
                        let t1 = web_time::Instant::now();
                        recover_edges_from_ast(&ast_for_recover, &mut diagram);
                        bevy::log::info!(
                            "[Projection] recover_edges done in {:.0}ms: {} edges",
                            t1.elapsed().as_secs_f64() * 1000.0,
                            diagram.edges.len(),
                        );
                        futures_lite::future::yield_now().await;
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
                    spawned_busy_label = Some((
                        match &target_class_snapshot {
                            Some(t) => format!("Projecting {t}"),
                            None => "Projecting…".to_string(),
                        },
                        target_class_snapshot.clone(),
                    ));
                    docstate.projection_task = Some(ProjectionTask {
                        gen_at_spawn: gen,
                        target_at_spawn: target_class_snapshot,
                        spawned_at,
                        deadline,
                        cancel,
                        task,
                        source_hash: source_hash_at_spawn,
                        _busy: None,
                    });
                }
            }
            // DO NOT update last_seen_gen here — only after the
            // task completes and the scene is actually swapped in.
            // Otherwise the `project_now` check on later frames
            // would think we're up-to-date and never swap.
            drop(state);

            // Attach a `BusyHandle` to the projection task we just
            // spawned (if any). Deferred two-step because the spawn
            // block above held `&mut CanvasDiagramState`; the bus and
            // state are disjoint resources but Rust can't see that
            // through `world`. The handle drops with the task on
            // completion / supersede / timeout.
            if let Some((label, target_after_spawn)) = spawned_busy_label {
                let handle = world
                    .resource_mut::<lunco_workbench::status_bus::StatusBus>()
                    .begin(
                        lunco_workbench::status_bus::BusyScope::Document(doc_id.0),
                        "projection",
                        label,
                    );
                let mut state = world.resource_mut::<CanvasDiagramState>();
                let docstate = match render_tab_id {
                    Some(t) => state.get_mut_for_tab(t, doc_id),
                    None => state.get_mut(Some(doc_id)),
                };
                // Only attach if the task we spawned is still the
                // current one (it could have been superseded between
                // the spawn block exit and here, though unlikely on
                // the synchronous frame). `target_at_spawn` matches
                // when it's the same task we just made.
                if let Some(task) = docstate.projection_task.as_mut() {
                    if task._busy.is_none()
                        && task.target_at_spawn == target_after_spawn
                    {
                        task._busy = Some(handle);
                    }
                }
            }
        }

        // Poll the in-flight projection task for the ACTIVE doc.
        // When it finishes, swap the scene in, update the sync
        // cursor, and (on first projection for this tab) frame the
        // scene with a sensible initial zoom.
        {
            // For task polling, the "active" doc is *this* tab's
            // doc — split panes each poll their own task slot.
            // active_doc_from_world prefers TabRenderContext.
            let active_doc = active_doc_from_world(world);
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
            let docstate = match (render_tab_id, active_doc) { (Some(t), Some(d)) => state.get_mut_for_tab(t, d), _ => state.get_mut(active_doc) };
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
            // the task — see `peek_or_load_msl_class_blocking`). During that
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
                    decorations::emit_diagram_decorations(&mut scene, &bg_graphics);
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
                    // Fit synchronously against the ui's pending
                    // allocation size — this is the same size
                    // `Canvas::ui` will hand to `allocate_exact_size`
                    // a few lines below, so the camera lands inside
                    // the actual canvas rect and the very first paint
                    // is already framed (no one-frame "wrong zoom"
                    // flash, no animated glide). Falls back to a
                    // physical-zoom snap at the origin when the scene
                    // is empty.
                    let physical_zoom =
                        lunco_canvas::Viewport::physical_mm_zoom(ui.ctx());
                    if let Some(world_rect) = docstate.canvas.scene.bounds() {
                        let avail = ui.available_size();
                        let origin = ui.cursor().min;
                        let screen = lunco_canvas::Rect::from_min_max(
                            lunco_canvas::Pos::new(origin.x, origin.y),
                            lunco_canvas::Pos::new(
                                origin.x + avail.x.max(1.0),
                                origin.y + avail.y.max(1.0),
                            ),
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
        // Logging policy:
        //  - Post-`apply_ops` window (`force_log`) → `debug!`. The
        //    window fires for 2 s after every structural edit and on
        //    a busy canvas every frame is over 8 ms, so this used to
        //    spam Console for the entire interactive session.
        //    `RUST_LOG=debug` still surfaces it when chasing
        //    apply-cost regressions.
        //  - Slow frames > 16 ms (one vsync budget) → `warn!`. The
        //    previous 8 ms threshold flagged every healthy egui
        //    frame on a busy canvas as "slow"; not actionable.
        if force_log {
            bevy::log::debug!(
                "[CanvasDiagram] frame: total={total_ms:.1}ms render_canvas={render_canvas_ms:.1}ms (post-apply window)"
            );
        } else if total_ms > 16.0 {
            bevy::log::warn!(
                "[CanvasDiagram] slow frame: total={total_ms:.1}ms render_canvas={render_canvas_ms:.1}ms"
            );
        }
    }
}

/// Wall-clock timestamp of the most recent `apply_ops` call. Used
/// by the post-Add window tracker in the panel render to log every
/// frame for ~2 seconds after each Add — captures sub-threshold
/// hitches that don't trip the SLOW frame log on their own but add
/// up to a perceived "freeze" when the user does something.
/// Process-shared cache of resolved port-connector icons.
///
/// Paint-hot path: `IconNodeVisual::paint` looks up each port's
/// connector icon many times per frame (once per port, sometimes
/// per candidate in a 3-tier scope chain). Hitting `engine.lock()`
/// inside paint locked the engine 30+ times per frame for PID-class
/// diagrams — main thread blocked for 100s of ms each frame.
///
/// This cache memoises `qualified_class → Option<Icon>` across all
/// frames, all nodes, all docs. Filled lazily on first lookup;
/// every subsequent paint hits a HashMap. Invalidated wholesale on
/// `DocumentChanged` via [`invalidate_port_icon_cache`] — coarse
/// but safe (cross-doc inheritance can shift on any edit; recompute
/// is cheap on rumoca's content-hash cache).
///
/// `Option<Icon>` so we cache "class has no icon" (Some(None)) as
/// a hit too — avoids re-walking a known-empty inheritance chain.
pub(super) fn port_icon_cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<crate::annotations::Icon>>>
{
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<crate::annotations::Icon>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}


/// Clear the process-shared port-icon cache. Called on
/// `DocumentChanged` so edits that move classes / change extends
/// chains don't keep returning stale icons.
pub(crate) fn invalidate_port_icon_cache() {
    port_icon_cache()
        .lock()
        .expect("port_icon_cache poisoned")
        .clear();
}

pub(super) static LAST_APPLY_AT: std::sync::Mutex<Option<web_time::Instant>> =
    std::sync::Mutex::new(None);

impl CanvasDiagramPanel {
    fn render_canvas(&self, ui: &mut egui::Ui, world: &mut World) {
        // Same per-render-call drill scope + tab id snapshot as the
        // parent `render` method.
        // `render_drilled` snapshot deleted — `render_tab_id`
        // alone scopes the lookup correctly. `ModelTabState` carries
        // the drilled class per-tab; nothing in this render path
        // needs the drilled name as a separate variable.
        let render_tab_id: Option<crate::ui::panels::model_view::TabId> = world
            .resource::<crate::ui::panels::model_view::TabRenderContext>()
            .tab_id;
        // Per-phase timing harness — gated on `RENDER_CANVAS_TRACE`
        // env var so the SLOW-frame log can pinpoint the heavy phase
        // without flooding normal runs. Set the var to anything
        // non-empty (`RENDER_CANVAS_TRACE=1`) to enable.
        let trace_phases = std::env::var_os("RENDER_CANVAS_TRACE").is_some();
        let mut phase_t = web_time::Instant::now();
        let mut phase_log: Vec<(&'static str, f64)> = Vec::new();
        let mark = |label: &'static str, t: &mut web_time::Instant, log: &mut Vec<(&'static str, f64)>| {
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
        let (doc_id, editing_class) = ops::resolve_doc_context(world);
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
        let tab_read_only = active_doc
            .map(|d| crate::ui::state::read_only_for(world, d))
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
            let docstate = match render_tab_id { Some(t) => state.get_mut_for_tab(t, d), None => state.get_mut(Some(d)) };
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
            // Layer order: experiment overlay first (final-time values
            // from the most recent visible Fast Run), then live cosim
            // values on top. When both exist the live values win,
            // which is what the user expects (live is actively
            // stepping; experiment is a snapshot).
            seed_state_from_latest_experiment(world, &mut state);
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
        // declared min/max bounds resolved via
        // [`crate::index::ModelicaIndex::find_component_by_leaf`]
        // (qualified-then-leaf precedence). Publishing this every
        // frame keeps the widgets responsive to recompiles,
        // parameter changes, and external writes.
        {
            let mut control_snapshot =
                lunco_viz::kinds::canvas_plot_node::InputControlSnapshot::default();
            if let Some(entity) = canvas_sim {
                if let Some(model) = world.get::<crate::ModelicaModel>(entity) {
                    let index_ref = world
                        .get_resource::<crate::ui::ModelicaDocumentRegistry>()
                        .and_then(|r| r.host(model.document))
                        .map(|h| h.document().index());
                    for (qualified, value) in &model.inputs {
                        let (mn, mx) = index_ref
                            .and_then(|idx| idx.find_component_by_leaf(qualified))
                            .map(|entry| (
                                entry.modifications.get("min").and_then(|s| s.parse().ok()),
                                entry.modifications.get("max").and_then(|s| s.parse().ok()),
                            ))
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
            let docstate = match (render_tab_id, active_doc) { (Some(t), Some(d)) => state.get_mut_for_tab(t, d), _ => state.get_mut(active_doc) };
            docstate.canvas.read_only = tab_read_only;
            docstate.canvas.snap = snap_settings;
            docstate.canvas.ui(ui)
        };
        mark("canvas.ui (scene render)", &mut phase_t, &mut phase_log);

        // R1: write the canvas-gesture flag from the response's
        // pointer-down state. egui's `is_pointer_button_down_on`
        // is true exactly while the user holds a button on this
        // widget — drag-in-progress is the canonical "mid-gesture"
        // signal we don't want autosave snapshotting through. Any
        // other source (text edit, modal) writes its own field on
        // the same resource.
        if let Some(mut active) = world.get_resource_mut::<crate::ui::wasm_autosave::IsGestureActive>() {
            active.canvas = response.is_pointer_button_down_on();
        }

        // Sibling-tab event replay was removed. After the
        // AST-canonical migration each mutation flows
        // canvas → host.apply → AST → source → DocumentChanged → next
        // frame's projection picks up the new gen and re-derives the
        // sibling scene. Letting projection be the single
        // synchronization point eliminates the per-event drift
        // (sibling and editing-tab scenes can no longer disagree).

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
                        // After A.4: drop emits one `AddComponent` op
                        // through `apply_ops_public`. The next-frame
                        // projection re-derives the scene from the
                        // gen-bumped AST. The legacy optimistic
                        // `synthesize_msl_node` path is gone; same-frame
                        // visual response now comes from the projector
                        // running unconditionally each tick.
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
                                .get_resource::<crate::ui::panels::model_view::ModelTabs>()
                                .and_then(|t| t.drilled_class_for_doc(doc_id))
                                .or_else(|| {
                                    let registry =
                                        world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
                                    let host = registry.host(doc_id)?;
                                    let ast = host.document().strict_ast()?;
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
                                ops::pick_add_instance_name(&def, &match render_tab_id { Some(t) => state.get_for_tab(t), None => state.get(Some(doc_id)) }.canvas.scene)
                            };
                            let op = ops::op_add_component_with_name(
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
            let docstate = match (render_tab_id, active_doc) { (Some(t), Some(d)) => state.get_mut_for_tab(t, d), _ => state.get_mut(active_doc) };
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
        let load_error: Option<(String, String)> = render_tab_id.and_then(|tid| {
            let tabs = world.resource::<crate::ui::panels::model_view::ModelTabs>();
            tabs.get(tid).and_then(|t| {
                t.load_error
                    .as_ref()
                    .map(|err| (t.drilled_class.clone().unwrap_or_default(), err.clone()))
            })
        });
        let (loading_info, projecting, parse_pending, show_empty_overlay, scene_has_content) = {
            let state = world.resource::<CanvasDiagramState>();
            let loads = world.resource::<DrillInLoads>();
            let dup_loads = world.resource::<DuplicateLoads>();
            let docstate = match render_tab_id { Some(t) => state.get_for_tab(t), None => state.get(active_doc) };
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
            // Parse pending = doc's syntax cache hasn't been populated
            // by the off-thread worker yet (or is behind the current
            // source generation). With the wasm worker-parse pipeline
            // every fresh tab spends a brief window in this state
            // before its AST lands; without an explicit overlay the
            // canvas just shows the "no diagram" empty card, which
            // looks broken. The flag piggybacks on `ast_is_stale()`
            // so we don't have to plumb the engine pending set into
            // the panel.
            let parse_pending = active_doc
                .and_then(|d| {
                    world
                        .resource::<crate::ui::ModelicaDocumentRegistry>()
                        .host(d)
                        .map(|h| h.document().ast_is_stale())
                })
                .unwrap_or(false);
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
                parse_pending,
                !has_content
                    && docstate.projection_task.is_none()
                    && !parse_pending,
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
        if let Some((class, err)) = load_error {
            overlays::render_drill_in_error_overlay(ui, response.rect, &class, &err, &theme_snapshot_for_overlay);
        } else if let Some((class, secs)) = loading_info {
            if !scene_has_content {
                overlays::render_drill_in_loading_overlay(ui, response.rect, &class, secs, &theme_snapshot_for_overlay);
            }
        } else if parse_pending && !scene_has_content {
            // Worker parse hasn't landed yet — show the same spinner
            // the projecting state uses. Without this branch the
            // user sees the "no diagram" empty card during the brief
            // gap between tab open and worker parse completion,
            // which reads as broken.
            overlays::render_projecting_overlay(ui, response.rect, &theme_snapshot_for_overlay);
            ui.ctx().request_repaint();
        } else if projecting && !scene_has_content {
            overlays::render_projecting_overlay(ui, response.rect, &theme_snapshot_for_overlay);
        } else if show_empty_overlay {
            overlays::render_empty_diagram_overlay(ui, response.rect, world);
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
        let popup_was_open_before = egui::Popup::is_any_open(ui.ctx());

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
                egui::Popup::close_all(ui.ctx());
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
                    egui::Popup::close_all(ui.ctx());
                    suppress_menu = true;
                } else {
                    // If egui still thinks a popup is open (from a
                    // previous tab), close it so this frame's
                    // `response.context_menu()` can open our fresh
                    // one without egui deduping against the stale
                    // popup id.
                    if popup_was_open_before {
                        egui::Popup::close_all(ui.ctx());
                    }
                    // Fresh right-click: capture world position +
                    // hit-test origin while `press_origin` still
                    // reflects the right-click (before any menu-entry
                    // click overwrites it).
                    let state = world.resource::<CanvasDiagramState>();
                    let docstate = match render_tab_id { Some(t) => state.get_for_tab(t), None => state.get(active_doc) };
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
                        menus::render_node_menu(
                            ui,
                            world,
                            *id,
                            editing_class.as_deref(),
                            &mut collected,
                        );
                    }
                    ContextMenuTarget::Edge(id) => {
                        menus::render_edge_menu(
                            ui,
                            world,
                            *id,
                            editing_class.as_deref(),
                            &mut collected,
                        );
                    }
                    ContextMenuTarget::Empty => {
                        menus::render_empty_menu(
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
        let popup_open_now = egui::Popup::is_any_open(ui.ctx());
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
            let mut all_ops = ops::build_ops_from_events(world, &events, class);
            all_ops.extend(menu_ops);
            if !all_ops.is_empty() {
                #[cfg(feature = "lunco-api")]
                crate::api_edits::trigger_apply_ops(world, doc_id, all_ops);
                #[cfg(not(feature = "lunco-api"))]
                apply_ops_public(world, doc_id, all_ops);
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

/// Seed the canvas's node-state snapshot from the most recently
/// completed visible Fast Run. Picks the newest visible experiment
/// in the registry, takes the final-time value for every variable
/// it recorded, and writes them to `state.values` keyed by the same
/// dotted Modelica path the live cosim path uses.
///
/// Lets the same edge-animation + hover-tooltip code light up the
/// diagram with a static snapshot of the run when no live cosim is
/// active. Live values overwrite these afterwards; on tabs with an
/// active stepper, this is invisible.
///
/// v1: takes the *final-time* sample (last entry in `times`). A
/// future time scrubber would parameterize the index; for now the
/// final value is the most useful default ("what does the system
/// settle at?").
fn seed_state_from_latest_experiment(
    world: &bevy::prelude::World,
    state: &mut lunco_viz::kinds::canvas_plot_node::NodeStateSnapshot,
) {
    use lunco_experiments::{ExperimentRegistry, TwinId};
    let twin = TwinId("default".into());
    let visibility = world
        .get_resource::<crate::ui::panels::experiments::ExperimentVisibility>();
    let active_plot = world
        .get_resource::<crate::ui::panels::experiments::ActivePlot>()
        .copied()
        .unwrap_or_default()
        .or_default();
    let plot_states = world
        .get_resource::<crate::ui::panels::experiments::PlotPanelStates>();
    let Some(registry) = world.get_resource::<ExperimentRegistry>() else {
        return;
    };
    // Pick the newest visible experiment with a result. The registry
    // preserves insertion order; iterate from the back for "newest
    // first".
    let exps = registry.list_for_twin(&twin);
    let chosen = exps.iter().rev().find(|e| {
        e.result.is_some()
            && visibility
                .map(|v| v.visible.contains(&e.id))
                .unwrap_or(true)
    });
    let Some(exp) = chosen else { return };
    let Some(result) = &exp.result else { return };
    if result.times.is_empty() {
        return;
    }
    // Scrub time wins over final-time when set. Find the sample
    // index whose time is closest to the user-picked scrub time so
    // the canvas reflects the system state at that moment.
    let scrub_time = plot_states.and_then(|s| s.scrub(active_plot));
    let idx = match scrub_time {
        Some(t) => {
            let mut best = 0usize;
            let mut best_d = f64::INFINITY;
            for (i, ti) in result.times.iter().enumerate() {
                let d = (ti - t).abs();
                if d < best_d {
                    best_d = d;
                    best = i;
                }
            }
            best
        }
        None => result.times.len() - 1,
    };
    for (name, samples) in &result.series {
        if let Some(v) = samples.get(idx) {
            if v.is_finite() {
                state.values.insert(name.clone(), *v);
            }
        }
    }
}