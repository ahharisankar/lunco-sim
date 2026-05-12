//! Resource state and result types for the Package Browser.

use bevy::prelude::*;
use bevy::tasks::Task;
use crate::ui::state::ModelLibrary;
use super::types::{PackageNode, InMemoryEntry, TwinNode};

pub struct ScanResult {
    pub parent_id: String,
    pub children: Vec<PackageNode>,
}

pub struct FileLoadResult {
    pub id: String,
    pub dedup_key: String,
    pub name: String,
    pub library: ModelLibrary,
    pub source: std::sync::Arc<str>,
    pub line_starts: std::sync::Arc<[usize]>,
    pub detected_name: Option<String>,
    pub layout_job: Option<bevy_egui::egui::text::LayoutJob>,
    pub doc_id: lunco_doc::DocumentId,
    pub doc: crate::document::ModelicaDocument,
}

#[derive(Clone)]
pub struct TwinState {
    pub root: std::path::PathBuf,
    pub root_node: TwinNode,
}

#[derive(Default, Clone)]
pub struct RenameState {
    pub target: Option<std::path::PathBuf>,
    pub buffer: String,
    pub needs_focus: bool,
}

#[derive(Resource)]
pub struct PackageTreeCache {
    pub roots: Vec<PackageNode>,
    pub tasks: Vec<Task<ScanResult>>,
    pub file_tasks: Vec<Task<FileLoadResult>>,
    pub loading_ids: std::collections::HashMap<String, lunco_doc::DocumentId>,
    pub in_memory_models: Vec<InMemoryEntry>,
    pub twin: Option<TwinState>,
    pub twin_scan_task: Option<Task<TwinState>>,
    pub rename: RenameState,
    pub bundled_tree_indexed: bool,
}

impl PackageTreeCache {
    pub fn is_loading(&self, doc: lunco_doc::DocumentId) -> bool {
        self.loading_ids.values().any(|d| *d == doc)
    }

    pub fn new() -> Self {
        let msl_root = lunco_assets::msl_dir();
        let modelica_dir = msl_root.join("Modelica");

        let mut roots = Vec::new();
        roots.push(PackageNode::Category {
            id: "msl_root".into(),
            name: "📚 Modelica Standard Library".into(),
            package_path: "Modelica".into(),
            fs_path: modelica_dir,
            children: None,
            is_loading: false,
        });

        // Third-party Modelica libraries discovered in the
        // `lunco-assets` cache. Each `Assets.toml` `dest = "<sub>"`
        // entry unpacks to `<cache>/<sub>/<PackageName>/package.mo`;
        // the discovery scan picks them up so adding a library is a
        // pure data change (download + Assets.toml entry).
        for (cache_subdir, package_dir) in super::scanner::discover_third_party_libs() {
            let lib_dir = lunco_assets::cache_dir().join(&cache_subdir).join(&package_dir);
            roots.push(PackageNode::Category {
                id: format!("{cache_subdir}_root"),
                name: package_dir.clone(),
                package_path: package_dir,
                fs_path: lib_dir,
                children: None,
                is_loading: false,
            });
        }

        // Bundled models — pre-baked tree from `msl_indexer`.
        roots.push(PackageNode::Category {
            id: "bundled_root".into(),
            name: "📦 Bundled Models".into(),
            package_path: "Bundled".into(),
            fs_path: std::path::PathBuf::new(),
            children: Some(build_bundled_tree()),
            is_loading: false,
        });

        let bundled_tree_indexed = !crate::visual_diagram::msl_bundled_trees().is_empty();

        Self {
            roots,
            tasks: Vec::new(),
            file_tasks: Vec::new(),
            loading_ids: std::collections::HashMap::new(),
            in_memory_models: Vec::new(),
            twin: None,
            twin_scan_task: None,
            rename: RenameState::default(),
            bundled_tree_indexed,
        }
    }
}

impl Default for PackageTreeCache {
    fn default() -> Self {
        Self::new()
    }
}

fn build_bundled_tree() -> Vec<PackageNode> {
    use crate::visual_diagram::{msl_bundled_trees, BundledClassTree};
    use crate::models::bundled_models;
    let trees = msl_bundled_trees();
    let trees_by_filename: std::collections::HashMap<&str, &BundledClassTree> =
        trees.iter().map(|t| (t.filename.as_str(), &t.top)).collect();
    bundled_models()
        .iter()
        .map(|m| match trees_by_filename.get(m.filename) {
            Some(tree) => bundled_class_to_node(m.filename, tree),
            None => PackageNode::Model {
                id: format!("bundled://{}", m.filename),
                name: m
                    .filename
                    .strip_suffix(".mo")
                    .unwrap_or(m.filename)
                    .to_string(),
                library: ModelLibrary::Bundled,
                class_kind: Some(crate::index::ClassKind::Model),
            },
        })
        .collect()
}

fn bundled_class_to_node(
    filename: &str,
    tree: &crate::visual_diagram::BundledClassTree,
) -> PackageNode {
    let is_package = matches!(tree.class_kind, crate::index::ClassKind::Package);
    if is_package && !tree.children.is_empty() {
        let children: Vec<PackageNode> = tree
            .children
            .iter()
            .map(|c| bundled_class_to_node(filename, c))
            .collect();
        PackageNode::Category {
            id: format!("bundled://{filename}#{}", tree.qualified_path),
            name: tree.short_name.clone(),
            package_path: tree.qualified_path.clone(),
            fs_path: std::path::PathBuf::new(),
            children: Some(children),
            is_loading: false,
        }
    } else {
        PackageNode::Model {
            id: format!("bundled://{filename}#{}", tree.qualified_path),
            name: tree.short_name.clone(),
            library: ModelLibrary::Bundled,
            class_kind: Some(tree.class_kind),
        }
    }
}
