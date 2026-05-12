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

        // Other roots would be added here
        roots.push(PackageNode::Category {
            id: "bundled_root".into(),
            name: "📦 Bundled Models".into(),
            package_path: "Bundled".into(),
            fs_path: std::path::PathBuf::new(),
            children: None,
            is_loading: false,
        });

        Self {
            roots,
            tasks: Vec::new(),
            file_tasks: Vec::new(),
            loading_ids: std::collections::HashMap::new(),
            in_memory_models: Vec::new(),
            twin: None,
            twin_scan_task: None,
            rename: RenameState::default(),
            bundled_tree_indexed: false,
        }
    }
}

impl Default for PackageTreeCache {
    fn default() -> Self {
        Self::new()
    }
}
