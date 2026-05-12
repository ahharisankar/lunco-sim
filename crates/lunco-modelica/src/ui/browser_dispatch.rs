//! Drains [`lunco_workbench::BrowserActions`] and routes each into the
//! appropriate Modelica subsystem.
//!
//! Sections push abstract intents (`OpenFile`, `OpenModelicaClass`)
//! during render; this system picks them up the same frame and turns
//! them into [`ClassRef`](crate::class_ref::ClassRef) values dispatched
//! to [`crate::ui::panels::package_browser::open_class`]. The drill-in
//! target rides directly on the `ClassRef`, so there is no out-of-band
//! queue to synchronise with the async source load — `ensure_for(doc,
//! drilled)` writes the tab state up front and the load task fills in
//! the source when it lands.

use bevy::prelude::*;
use lunco_workbench::{BrowserAction, BrowserActions};

/// Drain the Twin Browser action outbox each frame and dispatch.
///
/// Runs in `Update` after the panel render so actions queued during
/// the egui pass are picked up the same frame they were emitted.
pub fn drain_browser_actions(world: &mut World) {
    // Pull actions out of the world so we can mutate other resources
    // (DocumentRegistry, WorkbenchState, …) freely while iterating.
    let actions: Vec<BrowserAction> = {
        let mut outbox = world.resource_mut::<BrowserActions>();
        outbox.drain()
    };
    if actions.is_empty() {
        return;
    }

    // Resolve `relative_path` → absolute path against the currently-
    // active Twin's root. Captured once so we don't fight the borrow
    // checker re-borrowing `WorkspaceResource` per action.
    let twin_root = {
        let ws = world.resource::<lunco_workbench::WorkspaceResource>();
        ws.active_twin
            .and_then(|id| ws.twin(id))
            .map(|t| t.root.clone())
    };

    for action in actions {
        match action {
            BrowserAction::OpenFile { relative_path } => {
                let Some(root) = twin_root.as_ref() else {
                    log::warn!(
                        "BrowserAction::OpenFile fired with no active Twin: {:?}",
                        relative_path
                    );
                    continue;
                };
                let abs = root.join(&relative_path);
                let class = crate::class_ref::ClassRef::user_file(abs, Vec::<String>::new());
                crate::ui::panels::package_browser::open_class(world, class, false);
            }
            BrowserAction::OpenModelicaClass {
                relative_path,
                qualified_path,
            } => {
                let Some(root) = twin_root.as_ref() else {
                    log::warn!(
                        "BrowserAction::OpenModelicaClass fired with no active Twin: {:?}",
                        relative_path
                    );
                    continue;
                };
                let abs = root.join(&relative_path);
                let qualified_parts: Vec<String> = qualified_path
                    .split('.')
                    .map(String::from)
                    .collect();
                let class = crate::class_ref::ClassRef::user_file(abs, qualified_parts);
                crate::ui::panels::package_browser::open_class(world, class, false);
            }
            BrowserAction::OpenLoadedClass {
                doc_id,
                qualified_path,
            } => {
                let doc = lunco_doc::DocumentId::new(doc_id);
                // B.3 phase 3: `ensure_preview_for(doc,
                // Some(qualified_path))` writes the drilled scope
                // onto the tab — the tab table is now authoritative,
                // legacy `DrilledInClassNames` cache mirror removed.
                let (tab_id, evict) = {
                    let mut model_tabs = world
                        .resource_mut::<crate::ui::panels::model_view::ModelTabs>();
                    model_tabs.ensure_preview_for(doc, Some(qualified_path))
                };
                // ensure_preview_for never rebinds TabIds; an evicted
                // previous preview is closed here. Layout mutation goes
                // through CloseTab/OpenTab triggers because WorkbenchLayout
                // is removed from the World for the duration of rendering.
                if let Some(old_id) = evict {
                    world.commands().trigger(lunco_workbench::CloseTab {
                        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                        instance: old_id,
                    });
                    world
                        .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
                        .close_tab(old_id);
                    if let Some(mut state) = world
                        .get_resource_mut::<crate::ui::panels::canvas_diagram::CanvasDiagramState>()
                    {
                        state.drop_tab(old_id);
                    }
                }
                world.commands().trigger(lunco_workbench::OpenTab {
                    kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                    instance: tab_id,
                });
                // Make this doc the active workspace doc so the
                // canvas (which reads `WorkspaceResource::active_document`
                // to decide what to render) follows the click. Without
                // this, clicking a class in the entity tree only sets
                // `DrilledInClassNames[doc]` — but if the canvas was
                // rendering a different doc, the new drill target is
                // never observed and the diagram looks frozen.
                world
                    .resource_mut::<lunco_workbench::WorkspaceResource>()
                    .active_document = Some(doc);
                // Force a fresh projection on the next canvas tick —
                // the doc may have been already open at the package
                // (target=None) level, with a cached zero-node scene.
                world
                    .resource_mut::<crate::ui::state::WorkbenchState>()
                    .diagram_dirty = true;
            }
            // `BrowserAction` is `#[non_exhaustive]` upstream; future
            // variants land as warnings here, not silent drops.
            other => {
                log::warn!("unhandled BrowserAction: {:?}", other);
            }
        }
    }
}
