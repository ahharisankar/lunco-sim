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

/// Render the children of one named root in [`PackageTreeCache::roots`]
/// inside the Twin panel's per-library section. Lazily kicks off the
/// first scan if the root hasn't been populated yet; subsequent
/// renders walk the cached tree.
pub fn render_root_subtree(world: &mut World, ui: &mut egui::Ui, root_id: &str) {
    use bevy::tasks::AsyncComputeTaskPool;

    let active_path_str = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
        .and_then(|d| crate::ui::state::display_name_for(world, d));
    let active_path = active_path_str.as_deref();
    let theme = world
        .get_resource::<lunco_theme::Theme>()
        .cloned()
        .unwrap_or_else(lunco_theme::Theme::dark);

    let mut action: Option<render::PackageAction> = None;
    {
        let mut cache = world.resource_mut::<PackageTreeCache>();
        let cache = &mut *cache;
        let Some(root) = cache
            .roots
            .iter_mut()
            .find(|r| matches!(r, PackageNode::Category { id, .. } if id == root_id))
        else {
            return;
        };
        if let PackageNode::Category {
            id, package_path, fs_path, children, is_loading, ..
        } = root
        {
            ui.set_max_width(ui.available_width());
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            if let Some(kids) = children {
                for kid in kids.iter_mut() {
                    if let Some(a) = render::render_node_single(
                        kid, ui, active_path, None, 0, &mut cache.tasks, &theme,
                    ) {
                        action = Some(a);
                    }
                }
            } else if !*is_loading {
                *is_loading = true;
                let pool = AsyncComputeTaskPool::get();
                let parent_id = id.clone();
                let scan_dir = fs_path.clone();
                let pkg_path = package_path.clone();
                let task = pool.spawn(async move {
                    let children = crate::ui::panels::package_browser::scanner::scan_msl_dir(
                        &scan_dir, pkg_path,
                    );
                    crate::ui::panels::package_browser::cache::ScanResult { parent_id, children }
                });
                cache.tasks.push(task);
            }
            if *is_loading {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.label(
                        egui::RichText::new("⌛ Loading...")
                            .size(10.0)
                            .italics()
                            .color(egui::Color32::GRAY),
                    );
                });
            }
        }
    }

    if let Some(render::PackageAction::Open(id, name, lib, pinned)) = action {
        open_model(world, id, name, lib, pinned);
    } else if let Some(render::PackageAction::DragStart { msl_path }) = action {
        if let Some(def) = crate::visual_diagram::msl_component_by_path(&msl_path) {
            world
                .get_resource_or_insert_with::<crate::ui::panels::palette::ComponentDragPayload>(
                    Default::default,
                )
                .def = Some(def);
        }
    }
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

    // Bundled inner-class ids look like `bundled://<file>#<qualified>`;
    // the fragment is the drill-in target, the part before `#` is the
    // filename. MSL ids look like `msl_path:<qualified>` and resolve
    // their drill-in target via the qualified path itself once the
    // owning file lands.
    let drill_in_qualified: Option<String> = if id.starts_with("bundled://") {
        let tail = id.strip_prefix("bundled://").unwrap_or("");
        tail.split_once('#').map(|(_, q)| q.to_string())
    } else if let Some(rel) = id.strip_prefix("msl_path:") {
        Some(rel.to_string())
    } else {
        None
    };

    let path_buf = if let Some(rel) = id.strip_prefix("msl_path:") {
        resolve_msl_path_in_cache(world, rel).unwrap_or_else(|| PathBuf::from(&id))
    } else {
        PathBuf::from(&id)
    };

    let already_open = world.resource::<ModelicaDocumentRegistry>().find_by_path(&path_buf);
    if let Some(doc) = already_open {
        let tab_id = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>().ensure_for(doc, None);
        if let Some(qualified) = drill_in_qualified.clone() {
            if let Some(tab) = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>().get_mut(tab_id) {
                tab.drilled_class = Some(qualified);
            }
        }
        world.commands().trigger(lunco_workbench::OpenTab { kind: MODEL_VIEW_KIND, instance: tab_id });
        return;
    }

    let reserved_doc_id = world.resource_mut::<ModelicaDocumentRegistry>().reserve_id();
    world.resource_mut::<PackageTreeCache>().loading_ids.insert(id.clone(), reserved_doc_id);
    // Queue drill-in target so `handle_package_loading_tasks` can
    // attach it to the tab once the load completes.
    if let Some(qualified) = drill_in_qualified.clone() {
        world.resource_mut::<crate::ui::browser_dispatch::PendingDrillIns>().queue(id.clone(), qualified);
    }

    let tab_id = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>().ensure_for(reserved_doc_id, None);
    world.commands().trigger(lunco_workbench::OpenTab { kind: MODEL_VIEW_KIND, instance: tab_id });

    let writable = matches!(library, ModelLibrary::User);
    let origin = lunco_doc::DocumentOrigin::File { path: path_buf.clone(), writable };
    
    let id_clone = id.clone();
    let name_clone = name.clone();
    let lib_clone = library.clone();

    let task = AsyncComputeTaskPool::get().spawn(async move {
        let source_text = if id_clone.starts_with("bundled://") {
            let tail = id_clone.strip_prefix("bundled://").unwrap_or("");
            // Bundled inner-class ids look like `bundled://<file>#<qualified>`;
            // the `#frag` is consumed by the drill-in layer, the filename
            // lookup uses just the part before `#`.
            let filename = tail.split('#').next().unwrap_or(tail);
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

/// Resolve a `msl_path:` qualified name to the `.mo` file that owns
/// it by consulting [`PackageTreeCache::roots`]. Each library root
/// in the cache knows its own `package_path` (the qualified name of
/// the top-level class) and its on-disk `fs_path`, so we can resolve
/// uniformly for MSL, third-party libraries (`ThermofluidStream.*`),
/// or anything else that was registered as a root.
fn resolve_msl_path_in_cache(world: &World, rel_path: &str) -> Option<PathBuf> {
    let cache = world.resource::<PackageTreeCache>();
    let mut best: Option<(PathBuf, String, usize)> = None;
    for root in &cache.roots {
        if let PackageNode::Category { package_path, fs_path, .. } = root {
            if fs_path.as_os_str().is_empty() { continue; }
            // Match either the bare top-level name (e.g.
            // `ThermofluidStream`) or a qualified prefix
            // (`ThermofluidStream.Boundaries.X`).
            let matches = rel_path == package_path.as_str()
                || rel_path.starts_with(&format!("{package_path}."));
            if matches {
                let prefix_len = package_path.len();
                if best.as_ref().map(|(_, _, l)| prefix_len > *l).unwrap_or(true) {
                    best = Some((fs_path.clone(), package_path.clone(), prefix_len));
                }
            }
        }
    }
    let (fs_root, package_path, _) = best?;
    // Strip the matched prefix and walk down the remaining segments,
    // accepting either a sibling `.mo` file or a sub-package directory
    // at each step.
    let remainder = rel_path
        .strip_prefix(&package_path)
        .map(|s| s.trim_start_matches('.'))
        .unwrap_or("");
    let parts: Vec<&str> = if remainder.is_empty() { Vec::new() } else { remainder.split('.').collect() };
    for end in (0..=parts.len()).rev() {
        let mut as_file = fs_root.clone();
        for seg in &parts[..end] { as_file.push(seg); }
        if end > 0 {
            as_file.set_extension("mo");
            if as_file.is_file() { return Some(as_file); }
            as_file.set_extension("");
        }
        as_file.push("package.mo");
        if as_file.is_file() { return Some(as_file); }
    }
    None
}
