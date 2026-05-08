//! Canvas diagram operation builders + appliers.
//!
//! Translates user gestures (`lunco_canvas::CanvasEvent`) into
//! `ModelicaOp` writes against the active `Document`. Includes the
//! optimistic apply path used to keep the canvas in sync with the
//! document's per-class AST after every edit, plus the auto-arrange
//! observer that re-projects then runs the auto-layout pass.

use bevy::prelude::*;

use crate::document::ModelicaOp;
use crate::pretty::{self, Placement};
use crate::ui::state::{ModelicaDocumentRegistry, WorkbenchState};
use crate::visual_diagram::MSLComponentDef;

use super::coords::{ModelicaPos, canvas_to_modelica};
// B.3: `DrilledInClassNames` reads migrated to
// `crate::ui::panels::model_view::drilled_class_for_doc`.
use super::port::{port_fallback_offset_for_size, port_kind_str, resolve_port_icons};
use super::projection::{project_scene, projection_relevant_source_hash};
use super::{CanvasDiagramState, IconNodeData, active_doc_from_world};
use crate::ui::panels::model_view::TabRenderContext;

/// Read the active tab id from `TabRenderContext`. `None` outside a
/// panel render call (observers, off-render systems); call sites that
/// pair this with `get_for_render` correctly fall back to first-tab
/// semantics in that case.
fn render_tab_id(world: &World) -> Option<crate::ui::panels::model_view::TabId> {
    world
        .get_resource::<TabRenderContext>()
        .and_then(|c| c.tab_id)
}

/// Resolve `(document id, editing class name)` for the current tab.
/// Used by the canvas + neighbours so they target the same class when
/// `open_model` is bound.
pub(super) fn resolve_doc_context(world: &World) -> (Option<lunco_doc::DocumentId>, Option<String>) {
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
    // B.3: derive from `ModelTabs`.
    let drilled_in =
        crate::ui::panels::model_view::drilled_class_for_doc(world, doc_id);
    let open = world.resource::<WorkbenchState>().open_model.as_ref();
    let class = drilled_in
        .or_else(|| {
            world
                .resource::<ModelicaDocumentRegistry>()
                .host(doc_id)
                .and_then(|h| {
                    h.document()
                        .strict_ast()
                        .and_then(|ast| crate::ast_extract::extract_model_name_from_ast(&ast))
                })
        })
        .or_else(|| open.and_then(|o| o.detected_name.clone()));
    (Some(doc_id), class)
}

// Thin wrapper so existing call sites keep their shape. The real
// conversion lives in `super::coords::canvas_min_to_modelica_center`.

/// Translate canvas scene events into ModelicaOps. Needs a brief
/// read-only borrow of the scene (to look up edge endpoints); the
/// caller runs it inside its own borrow scope.
pub(super) fn build_ops_from_events(
    world: &mut World,
    events: &[lunco_canvas::SceneEvent],
    class: &str,
) -> Vec<ModelicaOp> {
    use lunco_canvas::SceneEvent;
    let active_doc = active_doc_from_world(world);
    let tab = render_tab_id(world);
    let state = world.resource::<CanvasDiagramState>();
    let scene = &state.get_for_render(tab, active_doc).canvas.scene;
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
                let m = super::coords::canvas_min_to_modelica_center(*new_min, icon_w, icon_h);
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
                let m = super::coords::canvas_min_to_modelica_center(new_rect.min, w, h);
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
pub(super) fn component_headers(
    world: &World,
    id: lunco_canvas::NodeId,
) -> (String, String) {
    let active_doc = active_doc_from_world(world);
    let tab = render_tab_id(world);
    let state = world.resource::<CanvasDiagramState>();
    let Some(node) = state.get_for_render(tab, active_doc).canvas.scene.node(id) else {
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
pub(super) fn pick_add_instance_name(comp: &MSLComponentDef, scene: &lunco_canvas::Scene) -> String {
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
pub(super) fn op_add_component_with_name(
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

// `synthesize_msl_node` — optimistic-scene helper — was deleted in
// A.4. Used to insert a Node into the canvas scene the same frame the
// op fired, ahead of the projection re-derivation. After A.2 the
// AST-canonical apply path is fast (no debounced reparse during
// apply) and the projection system runs every tick, so the next
// frame's projection picks up the new gen and renders the same node
// — no perceptible latency. Removing the optimistic path also kills
// a small drift class: the optimistic Node and the projected Node
// could disagree on port layout / icon rendering until the projector
// caught up.

pub(super) fn op_remove_component(
    world: &mut World,
    id: lunco_canvas::NodeId,
    class: &str,
) -> Option<ModelicaOp> {
    let active_doc = active_doc_from_world(world);
    let tab = render_tab_id(world);
    let state = world.resource::<CanvasDiagramState>();
    op_remove_node_inner(
        &state.get_for_render(tab, active_doc).canvas.scene,
        id,
        class,
    )
}

pub(super) fn op_remove_edge(
    world: &mut World,
    id: lunco_canvas::EdgeId,
    class: &str,
) -> Option<ModelicaOp> {
    let active_doc = active_doc_from_world(world);
    let tab = render_tab_id(world);
    let state = world.resource::<CanvasDiagramState>();
    op_remove_edge_inner(
        &state.get_for_render(tab, active_doc).canvas.scene,
        id,
        class,
    )
}

pub(super) fn op_remove_node_inner(
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

pub(super) fn op_remove_edge_inner(
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
    apply_ops(
        world,
        doc_id,
        ops,
        lunco_twin_journal::AuthorTag::local_user(),
    );
}

/// Variant of [`apply_ops_public`] that lets the caller specify the
/// author tag attached to journal entries. Used by API observers
/// (`tool: "api"`), agent-script bridges (`tool: "agent:<name>"`), and
/// future remote-replay paths. UI gestures should keep using
/// [`apply_ops_public`] (defaults to `local_user`).
pub fn apply_ops_as(
    world: &mut World,
    doc_id: lunco_doc::DocumentId,
    ops: Vec<ModelicaOp>,
    author: lunco_twin_journal::AuthorTag,
) {
    apply_ops(world, doc_id, ops, author);
}

/// Apply a single op through `host.apply` AND record the (forward,
/// inverse) pair to the canonical Twin journal in one funnel.
///
/// Replaces the direct-`host.apply` pattern that several API command
/// observers used to bypass the journal-recording path. Returns the
/// `host.apply` result so callers can branch on success/failure
/// exactly as before.
///
/// Side effects on success:
/// - `waive_ast_debounce()` (so the next AST refresh tick reparses
///   immediately, matching the canvas batch path).
/// - `registry.mark_changed(doc)` (queues a `DocumentChanged` event;
///   the previous direct-`host.apply` callers silently skipped this).
/// - One canonical journal entry recorded with the supplied `author`.
pub fn apply_one_op_as(
    world: &mut World,
    doc_id: lunco_doc::DocumentId,
    op: ModelicaOp,
    author: lunco_twin_journal::AuthorTag,
) -> Result<lunco_doc::Ack, lunco_doc::Reject> {
    use crate::ui::state::ModelicaDocumentRegistry;

    let forward = crate::journal::summarize_op(&op);
    // Layer 2 / structural ops can resolve a class that a previous
    // op in the same UI batch just created. Mirror the canvas batch
    // path's pre-apply AST refresh for those op kinds so direct
    // single-op API calls don't see a stale snapshot either.
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

    let (result, backward_summary) = {
        let Some(mut registry) = world.get_resource_mut::<ModelicaDocumentRegistry>() else {
            return Err(lunco_doc::Reject::InvalidOp(
                "ModelicaDocumentRegistry resource missing".into(),
            ));
        };
        let Some(host) = registry.host_mut(doc_id) else {
            return Err(lunco_doc::Reject::InvalidOp(format!(
                "doc {doc_id:?} not in registry"
            )));
        };
        if needs_fresh_ast {
            host.document_mut().refresh_ast_now();
        }
        let result = host.apply(op);
        let backward = if result.is_ok() {
            host.document_mut().waive_ast_debounce();
            host.last_applied_inverse().map(crate::journal::summarize_op)
        } else {
            None
        };
        if result.is_ok() {
            registry.mark_changed(doc_id);
        }
        (result, backward)
    };

    if let (Ok(_), Some(backward)) = (&result, backward_summary) {
        if let Some(journal) = world.get_resource::<lunco_doc_bevy::JournalResource>() {
            journal.with_write(|j| {
                j.record_op_value(
                    author,
                    doc_id,
                    lunco_twin_journal::DomainKind::Modelica,
                    forward,
                    backward,
                    None,
                );
            });
        }
    }
    result
}

pub(super) fn apply_ops(
    world: &mut World,
    doc_id: lunco_doc::DocumentId,
    ops: Vec<ModelicaOp>,
    author: lunco_twin_journal::AuthorTag,
) {
    // TEMP: timing instrumentation to find the source of the
    // multi-second lag observed when adding components from the
    // right-click menu. Each phase is timed independently so we
    // know which one to optimise.
    let t_start = web_time::Instant::now();
    // Auto-pin every tab pointing at this doc — VS Code semantics:
    // any edit to a preview tab promotes it to a permanent tab so
    // the next browser click doesn't replace it. Cheap (one
    // HashMap walk over open tabs).
    world
        .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
        .pin_all_for_doc(doc_id);
    // Stamp the post-apply window so the canvas frame logger
    // captures every subsequent frame's timing for ~2 seconds.
    if let Ok(mut guard) = super::panel::LAST_APPLY_AT.lock() {
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
                        let _ = crate::class_cache::peek_or_load_msl_class_blocking(&qualified);
                    })
                    .detach();
            }
        }
    }
    let preload_ms = 0.0_f64;

    let t_apply_start = web_time::Instant::now();
    // Captured (forward, inverse) op-summary pairs recorded into the
    // canonical Twin journal once the registry borrow drops. Built
    // inside the loop so each pair sees the inverse the host just
    // pushed onto its undo stack.
    let mut journal_pairs: Vec<(serde_json::Value, serde_json::Value)> = Vec::new();
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
            // Snapshot the forward summary BEFORE host.apply consumes the
            // op, so the journal records what the user intended even if
            // apply fails. (Failure paths drop the snapshot below.)
            let forward = crate::journal::summarize_op(&op);
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
                Ok(_) => {
                    any_applied = true;
                    // Inverse was just pushed onto the undo stack by
                    // DocumentHost::apply — read it and pair with the
                    // forward summary for the journal. Skipped on
                    // failure paths (forward summary is dropped).
                    let backward = host
                        .last_applied_inverse()
                        .map(crate::journal::summarize_op)
                        .unwrap_or(serde_json::Value::Null);
                    journal_pairs.push((forward, backward));
                }
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

    // Record the captured op pairs into the canonical Twin journal.
    // Single lock per batch keeps contention bounded; ordering matches
    // apply order so undo / replay walk the journal in the right
    // direction. Author flows in from the public entry point — UI
    // gestures default to `local_user`; API observers and agent
    // bridges pass their own tag.
    if !journal_pairs.is_empty() {
        if let Some(journal) = world.get_resource::<lunco_doc_bevy::JournalResource>() {
            journal.with_write(|j| {
                for (forward, backward) in journal_pairs.drain(..) {
                    j.record_op_value(
                        author.clone(),
                        doc_id,
                        lunco_twin_journal::DomainKind::Modelica,
                        forward,
                        backward,
                        None,
                    );
                }
            });
        }
    }

    if hit_read_only {
        // B.3 phase 4: per-doc error.
        if let Some(mut cs) = world.get_resource_mut::<crate::ui::CompileStates>() {
            // Don't clobber a real compile error.
            if cs.error_for(doc_id).is_none() {
                cs.set_error(
                    doc_id,
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
        // new generation tells the project gate "the scene already
        // reflects this state — don't re-project" — but **only for
        // the tab the user actually edited**. Other tabs viewing
        // the same doc (splits) have stale scenes and *do* need to
        // reproject; leaving their `last_seen_gen` untouched lets
        // the gen-advance check fire on their next render.
        let new_hash = projection_relevant_source_hash(&src);
        let editing_tab = world
            .resource::<crate::ui::panels::model_view::TabRenderContext>()
            .tab_id;
        // Ack the gen on the editing tab so its render loop won't
        // re-project (it already shows the new state). Sibling tabs
        // viewing the same `(doc, drilled)` are kept in sync via
        // [`apply_event_to_sibling_scene`] — replayed by the canvas
        // panel right after `canvas.ui()` returns events. Mutations
        // that don't have a SceneEvent equivalent (menu add /
        // remove, palette drop) fall through to gen-advance on the
        // sibling's next render, which reprojects from the
        // freshly-rewritten source.
        if let Some(mut state) = world.get_resource_mut::<CanvasDiagramState>() {
            if let Some(tab_id) = editing_tab {
                let docstate = state.get_mut_for_tab(tab_id, doc_id);
                docstate.canvas_acked_gen = new_gen;
                docstate.last_seen_gen = new_gen;
                docstate.last_seen_source_hash = new_hash;
            } else {
                // Non-render-context dispatch (API, observer);
                // legacy single-tab path.
                let docstate = state.get_mut(Some(doc_id));
                docstate.canvas_acked_gen = new_gen;
                docstate.last_seen_gen = new_gen;
                docstate.last_seen_source_hash = new_hash;
            }
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

pub(super) fn auto_arrange_now(world: &mut World, doc_id: lunco_doc::DocumentId) {
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
            let m = super::coords::canvas_min_to_modelica_center(
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
    apply_ops_public(world, doc_id, ops);
}

/// Resolve the active class name for an Auto-Arrange target. Prefers
/// the drilled-in class name (for MSL drill-in tabs); falls back to
/// the open document's detected model name.
pub(super) fn active_class_for_doc(world: &mut World, doc_id: lunco_doc::DocumentId) -> Option<String> {
    // B.3: derive from `ModelTabs`.
    if let Some(c) = crate::ui::panels::model_view::drilled_class_for_doc(world, doc_id) {
        return Some(c);
    }
    world
        .get_resource::<WorkbenchState>()
        .and_then(|ws| ws.open_model.as_ref())
        .and_then(|o| o.detected_name.clone())
}