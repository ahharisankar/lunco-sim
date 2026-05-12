//! Tree node types and basic structures for the Package Browser.

use crate::ui::state::ModelLibrary;

#[derive(Debug, Clone)]
pub enum PackageNode {
    Category {
        id: String,
        name: String,
        /// Modelica dot-path (e.g. "Modelica.Electrical.Analog")
        package_path: String,
        /// Real filesystem path
        fs_path: std::path::PathBuf,
        /// None means not yet scanned. Some(vec![]) means scanned and empty.
        children: Option<Vec<PackageNode>>,
        /// Whether a background scan is currently in progress.
        is_loading: bool,
    },
    Model {
        id: String,
        name: String,
        library: ModelLibrary,
        /// Modelica class kind (`"model"`, `"block"`, `"connector"`,
        /// ...) peeked from the file's first non-comment, non-`within`
        /// keyword.
        class_kind: Option<String>,
    },
}

impl PackageNode {
    pub fn name(&self) -> &str {
        match self {
            PackageNode::Category { name, .. } | PackageNode::Model { name, .. } => name,
        }
    }
}

/// Tracks one in-memory ("scratch") model the user has created this
/// session.
#[derive(Debug, Clone)]
pub struct InMemoryEntry {
    pub display_name: String,
    pub id: String,
    pub doc: lunco_doc::DocumentId,
}

#[derive(Clone)]
pub struct TwinNode {
    pub path: std::path::PathBuf,
    pub name: String,
    pub children: Vec<TwinNode>,
    pub is_modelica: bool,
}
