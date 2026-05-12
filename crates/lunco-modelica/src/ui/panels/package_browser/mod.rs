//! Package Browser — Dymola-style library tree.

use bevy::prelude::*;
use bevy_egui::egui;
use crate::ui::state::{ModelicaDocumentRegistry, ModelLibrary, WorkbenchState};
use std::path::{PathBuf};

pub mod types;
pub mod cache;
pub mod scanner;
pub mod render;

pub use types::{PackageNode, InMemoryEntry};
pub use cache::PackageTreeCache;
pub use render::PackageBrowserPanel;
pub use scanner::{scan_twin_folder, discover_third_party_libs};

pub struct PackageBrowserPlugin;

impl Plugin for PackageBrowserPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PackageTreeCache>()
            .add_systems(Update, handle_package_loading_tasks);
    }
}

pub fn handle_package_loading_tasks(
    mut cache: ResMut<PackageTreeCache>,
    mut workbench: ResMut<WorkbenchState>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut model_tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut pending_drill_ins: ResMut<crate::ui::browser_dispatch::PendingDrillIns>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
) {
    use futures_lite::future;

    let mut finished_results = Vec::new();
    cache.tasks.retain_mut(|task| {
        if let Some(result) = future::block_on(future::poll_once(task)) {
            finished_results.push(result);
            false
        } else {
            true
        }
    });

    for result in finished_results {
        find_and_update_node(&mut cache.roots, &result.parent_id, result.children);
    }

    let mut finished_files = Vec::new();
    cache.file_tasks.retain_mut(|task| {
        if let Some(result) = future::block_on(future::poll_once(task)) {
            finished_files.push(result);
            false
        } else {
            true
        }
    });

    if let Some(mut task) = cache.twin_scan_task.take() {
        if let Some(scanned) = future::block_on(future::poll_once(&mut task)) {
            cache.twin = Some(scanned);
        } else {
            cache.twin_scan_task = Some(task);
        }
    }

    for result in finished_files {
        cache.loading_ids.remove(&result.id);
        let doc_id = result.doc_id;
        registry.install_prebuilt(doc_id, result.doc);

        let queued_qualified = pending_drill_ins.take(&result.id);
        if let Some(qualified) = queued_qualified {
            if let Some(tab) = model_tabs.find_for_mut(doc_id, None) {
                tab.drilled_class = Some(qualified);
            }
        }

        workbench.diagram_dirty = true;
        workspace.active_document = Some(doc_id);
    }
}

fn find_and_update_node(nodes: &mut [PackageNode], parent_id: &str, children: Vec<PackageNode>) -> bool {
    for node in nodes {
        match node {
            PackageNode::Category { id, children: node_children, is_loading, .. } => {
                if id == parent_id {
                    *node_children = Some(children);
                    *is_loading = false;
                    return true;
                }
                if let Some(ref mut sub_children) = node_children {
                    if find_and_update_node(sub_children, parent_id, children.clone()) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

pub fn render_root_subtree(_world: &mut World, _ui: &mut egui::Ui, _root_id: &str) {
    // FIXME: implement
}

pub(crate) fn open_model(
    world: &mut World,
    id: String,
    name: String,
    library: ModelLibrary,
    _pinned: bool,
) {
    use crate::ui::panels::model_view::MODEL_VIEW_KIND;
    use bevy::tasks::AsyncComputeTaskPool;

    let path_buf = if let Some(rel) = id.strip_prefix("msl_path:") {
        resolve_msl_path_to_file(rel).unwrap_or_else(|| PathBuf::from(&id))
    } else {
        PathBuf::from(&id)
    };

    let already_open = world.resource::<ModelicaDocumentRegistry>().find_by_path(&path_buf);
    if let Some(doc) = already_open {
        let tab_id = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>().ensure_for(doc, None);
        world.commands().trigger(lunco_workbench::OpenTab { kind: MODEL_VIEW_KIND, instance: tab_id });
        return;
    }

    let reserved_doc_id = world.resource_mut::<ModelicaDocumentRegistry>().reserve_id();
    world.resource_mut::<PackageTreeCache>().loading_ids.insert(id.clone(), reserved_doc_id);

    let tab_id = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>().ensure_for(reserved_doc_id, None);
    world.commands().trigger(lunco_workbench::OpenTab { kind: MODEL_VIEW_KIND, instance: tab_id });

    let writable = matches!(library, ModelLibrary::User);
    let origin = lunco_doc::DocumentOrigin::File { path: path_buf.clone(), writable };
    
    let id_clone = id.clone();
    let name_clone = name.clone();
    let lib_clone = library.clone();

    let task = AsyncComputeTaskPool::get().spawn(async move {
        let source_text = if id_clone.starts_with("bundled://") {
            let filename = id_clone.strip_prefix("bundled://").unwrap_or("");
            crate::models::get_model(filename).unwrap_or("").to_string()
        } else {
            std::fs::read_to_string(&path_buf).unwrap_or_default()
        };

        let doc = crate::document::ModelicaDocument::with_origin(reserved_doc_id, source_text.clone(), origin);
        let dedup_key = id_clone.clone();
        crate::ui::panels::package_browser::cache::FileLoadResult {
            id: id_clone,
            dedup_key,
            name: name_clone,
            library: lib_clone,
            source: source_text.into(),
            line_starts: vec![0].into(),
            detected_name: None,
            layout_job: None,
            doc_id: reserved_doc_id,
            doc,
        }
    });

    world.resource_mut::<PackageTreeCache>().file_tasks.push(task);
}

fn resolve_msl_path_to_file(rel_path: &str) -> Option<PathBuf> {
    let msl_root = lunco_assets::msl_dir();
    let parts: Vec<&str> = rel_path.split('.').collect();
    for end in (1..=parts.len()).rev() {
        let mut as_file = msl_root.join("Modelica");
        as_file.push(parts[..end].join("/"));
        as_file.set_extension("mo");
        if as_file.exists() { return Some(as_file); }
        let mut as_pkg = msl_root.join("Modelica");
        as_pkg.push(parts[..end].join("/"));
        as_pkg.push("package.mo");
        if as_pkg.exists() { return Some(as_pkg); }
    }
    None
}
