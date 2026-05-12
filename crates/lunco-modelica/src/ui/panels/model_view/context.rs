//! Helpers for syncing tab state with global workspace state.

use bevy::prelude::*;
use lunco_doc::DocumentId;
use crate::ui::panels::code_editor::EditorBufferState;
use crate::ui::{ModelicaDocumentRegistry, WorkbenchState};
use super::types::{TabId, TabRenderContext};
use super::tabs::ModelTabs;

pub fn drilled_class_for_doc(
    world: &World,
    doc: DocumentId,
) -> Option<String> {
    if let Some(ctx) = world.get_resource::<TabRenderContext>() {
        if let Some(tab_id) = ctx.tab_id {
            let tabs = world.resource::<ModelTabs>();
            if let Some(state) = tabs.get(tab_id) {
                if state.doc == doc {
                    return state.drilled_class.clone();
                }
            }
        }
    }
    world.resource::<ModelTabs>().drilled_class_for_doc(doc)
}

pub fn resolve_tab_target(world: &World, instance: u64) -> (DocumentId, Option<String>) {
    if let Some(state) = world.get_resource::<ModelTabs>().and_then(|t| t.get(instance)) {
        return (state.doc, state.drilled_class.clone());
    }
    (DocumentId::new(instance), None)
}

pub fn resolve_tab_title(
    world: &World,
    doc: DocumentId,
    drilled_class: Option<&str>,
) -> (String, bool, bool) {
    if let Some(host) = world
        .get_resource::<ModelicaDocumentRegistry>()
        .and_then(|r| r.host(doc))
    {
        let document = host.document();
        let base = drilled_class
            .and_then(|qualified| qualified.rsplit('.').next().map(str::to_string))
            .unwrap_or_else(|| {
                let raw = document.origin().display_name();
                if raw == "package" {
                    if let lunco_doc::DocumentOrigin::File { path, .. } =
                        document.origin()
                    {
                        if let Some(parent) = path
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                        {
                            return parent.to_string();
                        }
                    }
                }
                raw
            });
        return (base, document.is_dirty(), document.is_read_only());
    }

    let active_doc = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document);
    if active_doc == Some(doc) {
        if let Some(name) = crate::ui::state::display_name_for(world, doc) {
            return (name, false, crate::ui::state::read_only_for(world, doc));
        }
    }
    (format!("Model #{}", doc.raw()), false, false)
}

pub fn sync_active_tab_to_doc(
    world: &mut World,
    doc: DocumentId,
    _drilled_class: Option<&str>,
) {
    let active_matches = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
        == Some(doc);
    let buffer_matches_live = {
        let live = world
            .resource::<ModelicaDocumentRegistry>()
            .host(doc)
            .map(|h| h.document().source().len())
            .unwrap_or(0);
        let buf_len = world
            .get_resource::<EditorBufferState>()
            .map(|b| b.text.len())
            .unwrap_or(0);
        live > 0 && live == buf_len
    };
    if active_matches && buffer_matches_live {
        refresh_selected_entity_for(world, doc);
        return;
    }

    let snapshot = {
        let registry = world.resource::<ModelicaDocumentRegistry>();
        registry.host(doc).map(|h| {
            let document = h.document();
            let display_name = document.origin().display_name();
            let path_str = document
                .canonical_path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("mem://{display_name}"));
            let library = match document.origin() {
                lunco_doc::DocumentOrigin::Untitled { .. } => {
                    crate::ui::state::ModelLibrary::InMemory
                }
                lunco_doc::DocumentOrigin::File { writable: true, .. } => {
                    crate::ui::state::ModelLibrary::User
                }
                lunco_doc::DocumentOrigin::File { writable: false, .. } => {
                    crate::ui::state::ModelLibrary::Bundled
                }
            };
            let read_only =
                matches!(library, crate::ui::state::ModelLibrary::Bundled);
            let detected_name = document
                .index()
                .classes
                .values()
                .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                .map(|c| c.name.clone());
            (
                path_str,
                display_name,
                document.source().to_string(),
                read_only,
                library,
                detected_name,
            )
        })
    };

    let snapshot = snapshot.or_else(|| {
        if let Some(loads) = world
            .get_resource::<crate::ui::panels::canvas_diagram::DrillInLoads>()
        {
            if let Some(qualified) = loads.detail(doc) {
                let qualified = qualified.to_string();
                let short = qualified
                    .rsplit('.')
                    .next()
                    .map(str::to_string)
                    .unwrap_or_else(|| qualified.clone());
                return Some((
                    format!("msl://{qualified}"),
                    short.clone(),
                    String::new(),
                    true,
                    crate::ui::state::ModelLibrary::Bundled,
                    Some(short),
                ));
            }
        }
        if let Some(dup) = world
            .get_resource::<crate::ui::panels::canvas_diagram::DuplicateLoads>()
        {
            if let Some(display) = dup.detail(doc) {
                let display = display.to_string();
                return Some((
                    format!("mem://{display}"),
                    display.clone(),
                    String::new(),
                    false,
                    crate::ui::state::ModelLibrary::InMemory,
                    Some(display),
                ));
            }
        }
        None
    });
    let Some((path_str, display_name, source, read_only, library, detected_name)) =
        snapshot
    else {
        return;
    };

    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in source.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(i + 1);
        }
    }

    let _ = (display_name, read_only, library);
    {
        let source_arc: std::sync::Arc<str> = source.clone().into();
        let mut state = world.resource_mut::<WorkbenchState>();
        state.editor_buffer = source_arc.to_string();
        state.diagram_dirty = true;
    }

    {
        let mut ws = world.resource_mut::<lunco_workbench::WorkspaceResource>();
        ws.active_document = Some(doc);
    }

    {
        let mut buf = world.resource_mut::<EditorBufferState>();
        buf.text = source;
        buf.line_starts = line_starts.into();
        buf.detected_name = detected_name;
        buf.model_path = path_str;
        buf.bound_doc = Some(doc);
    }

    refresh_selected_entity_for(world, doc);
}

pub fn refresh_selected_entity_for(world: &mut World, doc: DocumentId) {
    let entity = world
        .resource::<ModelicaDocumentRegistry>()
        .entities_linked_to(doc)
        .into_iter()
        .next();
    if let Some(entity) = entity {
        if let Some(mut state) = world.get_resource_mut::<WorkbenchState>() {
            if state.selected_entity != Some(entity) {
                state.selected_entity = Some(entity);
            }
        }
    }
}
