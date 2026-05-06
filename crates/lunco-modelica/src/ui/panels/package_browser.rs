//! Package Browser — Dymola-style library tree.
//!
//! Scans the real MSL directory from disk (via `lunco_assets::msl_dir()`).
//! Bundled models are included as read-only entries.
//! Clicking any `.mo` file opens it in the Code Editor + Diagram panels.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_workbench::{Panel, PanelId, PanelSlot};

use crate::models::bundled_models;
use crate::ui::state::{ModelicaDocumentRegistry, ModelLibrary, OpenModel, WorkbenchState};

use bevy::tasks::{AsyncComputeTaskPool, Task};
use futures_lite::future;
use lunco_doc::DocumentId;

// ---------------------------------------------------------------------------
// Tree Nodes
// ---------------------------------------------------------------------------

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
        /// keyword. `None` only for nodes whose source we haven't
        /// scanned yet (Bundled today; future remote backends). The
        /// static `MSLComponentDef` index is consulted first; this
        /// field is the fallback so the kind badge stays correct for
        /// classes the static index doesn't cover (records, types,
        /// constants, partial-model siblings, ...). Cheap one-shot
        /// read at scan time — never re-read during render.
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

// ---------------------------------------------------------------------------
// Cached Tree
// ---------------------------------------------------------------------------

pub struct ScanResult {
    pub parent_id: String,
    pub children: Vec<PackageNode>,
}

pub struct FileLoadResult {
    pub id: String,
    pub name: String,
    pub library: ModelLibrary,
    pub source: std::sync::Arc<str>,
    pub line_starts: std::sync::Arc<[usize]>,
    pub detected_name: Option<String>,
    pub layout_job: Option<bevy_egui::egui::text::LayoutJob>,
    /// Pre-allocated document id and fully-built `ModelicaDocument`
    /// from the bg task. The heavy work (rumoca parse + AST cache
    /// fill) runs off the UI thread; the main thread only performs
    /// the cheap `install_prebuilt` HashMap insert. Without this
    /// the parse used to block egui for hundreds of ms on
    /// MSL-package-sized files (Rotational, MultiBody) — a clear
    /// violation of the "Update systems short" mandate.
    pub doc_id: lunco_doc::DocumentId,
    pub doc: crate::document::ModelicaDocument,
}

/// Tracks one in-memory ("scratch") model the user has created this
/// session. The document itself lives in [`ModelicaDocumentRegistry`];
/// this is the Package Browser's view of it (display name + id).
#[derive(Debug, Clone)]
pub struct InMemoryEntry {
    /// Human-readable name (matches the `model <name>` declaration).
    pub display_name: String,
    /// The `mem://<name>` id used as a stable `OpenModel.model_path`.
    pub id: String,
    /// DocumentId in the registry — source of truth for the model's text.
    /// Kept for direct lookups (close-entry, duplicate, etc.); the
    /// re-open path currently resolves via `find_by_path(id)` and
    /// doesn't strictly need this field.
    #[allow(dead_code)]
    pub doc: DocumentId,
}

#[derive(Resource)]
pub struct PackageTreeCache {
    pub roots: Vec<PackageNode>,
    /// Active scanning tasks.
    pub tasks: Vec<Task<ScanResult>>,
    /// Active file loading tasks.
    pub file_tasks: Vec<Task<FileLoadResult>>,
    /// Ids currently being loaded — guards against the user
    /// double- / triple-clicking a row while the bg parse is still
    /// running. Without this, every re-click spawns a fresh task
    /// and ends up opening a duplicate tab once each completes.
    /// Inserted before `spawn`, removed in `handle_package_loading_tasks`
    /// when the task finishes (success or failure). Maps id →
    /// reserved doc_id so a re-click can also re-focus the
    /// pre-opened placeholder tab.
    pub loading_ids: std::collections::HashMap<String, lunco_doc::DocumentId>,
    /// In-memory models created via "New Model…" this session. Listed
    /// under "Your Models" so the user can click back into one after
    /// they've navigated away.
    pub in_memory_models: Vec<InMemoryEntry>,
    /// Currently-open Twin folder (if any) + its scanned file tree.
    /// Populated by the "Open Folder" button, cleared by "Close Twin".
    pub twin: Option<TwinState>,
    /// In-flight async scan of a just-picked folder. Polled by
    /// `handle_package_loading_tasks`. While set, the Twin section
    /// shows a spinner so the UI never freezes.
    pub twin_scan_task: Option<Task<TwinState>>,
    /// Path currently being renamed (if any) + its edit buffer.
    pub rename: RenameState,
    /// Whether the bundled tree has been rebuilt with the indexer's
    /// per-file class trees. `PackageTreeCache::new()` runs at app
    /// startup — on wasm that's *before* the MSL bundle (which
    /// carries `msl_index.json`) finishes fetching, so the first
    /// pass falls back to flat-leaf rendering of every bundled file.
    /// `handle_package_loading_tasks` flips this to `true` once it
    /// re-runs `build_bundled_tree()` against a populated index, so
    /// LunCo Examples expand into their inner classes (Engine,
    /// Tank, RocketStage, …) instead of staying as opaque leaves.
    pub bundled_tree_indexed: bool,
}

/// User's Twin workspace — a folder on disk being browsed as a tree.
///
/// Read-only in this first pass: scanning + open-on-click. Edits
/// (new/rename/delete, drag-move) land in the next phase.
#[derive(Clone)]
pub struct TwinState {
    /// Root folder the user picked via Open Folder.
    pub root: std::path::PathBuf,
    /// Recursive tree of files + subfolders under `root`.
    pub root_node: TwinNode,
}

/// Transient rename state — which path is in rename mode + the
/// buffer the user is typing into. Lives on the cache so
/// render-state survives frame boundaries.
#[derive(Default, Clone)]
pub struct RenameState {
    /// The tree entry the user invoked Rename on.
    pub target: Option<std::path::PathBuf>,
    /// Current buffer (defaults to the original name on entry).
    pub buffer: String,
    /// When Some, the inline TextEdit should steal focus this frame
    /// (first frame of the rename — so the user can immediately type).
    pub needs_focus: bool,
}

/// One file or folder inside a Twin. Tree-shaped so `CollapsingHeader`
/// renders it cleanly (one level of nesting per depth step).
#[derive(Clone)]
pub struct TwinNode {
    /// Absolute path on disk.
    pub path: std::path::PathBuf,
    /// Display name — just the file/folder name.
    pub name: String,
    /// Directory nodes have `children`; file nodes have an empty vec.
    pub children: Vec<TwinNode>,
    /// True for `.mo` files (clickable, opens a tab). Other files
    /// are rendered greyed out / non-clickable so users see the
    /// structure but don't accidentally try to open non-Modelica docs.
    pub is_modelica: bool,
}

impl TwinNode {
    fn is_dir(&self) -> bool {
        // An empty file looks like an empty dir; distinguish by
        // file-system check on the path.
        !self.children.is_empty() || self.path.is_dir()
    }
}

impl PackageTreeCache {
    pub fn new() -> Self {
        let msl_root = lunco_assets::msl_dir();
        let modelica_dir = msl_root.join("Modelica");

        let mut roots = Vec::new();

        roots.push(PackageNode::Category {
            id: "msl_root".into(),
            name: "📚 Modelica Standard Library".into(),
            package_path: "Modelica".into(),
            fs_path: modelica_dir,
            children: None, // Will be loaded lazily
            is_loading: false,
        });

        // Extra third-party Modelica libraries discovered in the
        // `lunco-assets` cache. Each `Assets.toml` `dest = "<sub>"`
        // entry unpacks to `<cache>/<sub>/<PackageName>/package.mo`;
        // the discovery scan picks them up so adding a library is a
        // pure data change (download + Assets.toml entry) — no code
        // edit needed in this file. Pairs with `msl_indexer.rs`
        // (palette indexing) and `class_cache.rs` (drill-in path
        // resolution), which still need their own entries.
        for (cache_subdir, package_dir) in discover_third_party_libs() {
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

        roots.push(PackageNode::Category {
            id: "bundled_root".into(),
            name: "📦 Bundled Models".into(),
            package_path: "Bundled".into(),
            fs_path: std::path::PathBuf::new(),
            children: Some(build_bundled_tree()),
            is_loading: false,
        });

        roots.push(PackageNode::Category {
            id: "folder_root".into(),
            name: "📁 Open Folder".into(),
            package_path: "User".into(),
            fs_path: std::path::PathBuf::new(),
            children: Some(vec![PackageNode::Category {
                id: "folder_empty".into(),
                name: "(no folder open)".into(),
                package_path: "User.Empty".into(),
                fs_path: std::path::PathBuf::new(),
                children: Some(vec![]),
                is_loading: false,
            }]),
            is_loading: false,
        });

        // Native: msl_index.json is on disk synchronously at build
        // time, so `build_bundled_tree()` above already returned the
        // indexed shape — no rebuild needed. Wasm: bundle still
        // fetching, so the eager call returned flat leaves; mark for
        // rebuild once `handle_package_loading_tasks` sees the
        // indexer settle.
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

/// Recursively scan `root` into a [`TwinNode`] tree.
/// Skips hidden dirs, `.git`, common build / dependency caches.
/// Synchronous — callers run this on a background task so the UI
/// never blocks.
pub fn scan_twin_folder(root: std::path::PathBuf) -> TwinState {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let root_node = TwinNode {
        children: scan_children(&root),
        path: root.clone(),
        name,
        is_modelica: false,
    };
    TwinState { root, root_node }
}

fn scan_children(dir: &std::path::Path) -> Vec<TwinNode> {
    let Ok(iter) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in iter.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if should_skip(&name) {
            continue;
        }
        let path = entry.path();
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        let is_modelica = !is_dir
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mo"))
                .unwrap_or(false);
        let children = if is_dir { scan_children(&path) } else { Vec::new() };
        out.push(TwinNode {
            path,
            name,
            children,
            is_modelica,
        });
    }
    // Directories first, then files, each alphabetically. Standard
    // explorer ordering — nothing sadder than a file tree with files
    // interleaved with folders in creation order.
    out.sort_by(|a, b| {
        let a_dir = !a.children.is_empty() || a.path.is_dir();
        let b_dir = !b.children.is_empty() || b.path.is_dir();
        b_dir.cmp(&a_dir).then_with(|| a.name.cmp(&b.name))
    });
    out
}

fn should_skip(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "target" | "shared_target" | "node_modules" | "__pycache__"
        )
}

// ---------------------------------------------------------------------------
// MSL Tree Builder — scans real .mo files from disk
// ---------------------------------------------------------------------------

fn scan_msl_dir(dir: &std::path::Path, package_path: String) -> Vec<PackageNode> {
    // Wasm: enumerate from the in-memory MSL bundle instead of the
    // filesystem (which doesn't exist). The bundle is a `HashMap<
    // PathBuf, Vec<u8>>` keyed by the same relative paths the
    // filesystem would have (`Modelica/Blocks/package.mo`,
    // `Modelica/Blocks/Discrete.mo`, …), so the same package_path
    // → tree-node mapping applies — we just read the listing from
    // hashmap keys instead of `read_dir`.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = dir;
        return scan_msl_inmem(&package_path);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        scan_msl_dir_native(dir, package_path)
    }
}

#[cfg(target_arch = "wasm32")]
fn scan_msl_inmem(package_path: &str) -> Vec<PackageNode> {
    use std::collections::HashSet;
    use std::path::PathBuf;

    let Some(source) = lunco_assets::msl::global_msl_source() else {
        return Vec::new();
    };
    let in_mem = match source {
        lunco_assets::msl::MslAssetSource::InMemory(m) => m,
        // Filesystem-backed `MslAssetSource` shouldn't occur on wasm
        // (no fs), but bail safely if it does.
        _ => return Vec::new(),
    };

    // Translate the dotted package path to a slash-prefixed key.
    // Empty package_path is the bundle root (rare; only the very
    // top-level call before "Modelica" was pushed).
    let prefix: String = package_path.replace('.', "/");
    let prefix_with_slash = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix}/")
    };

    let mut results: Vec<PackageNode> = Vec::new();
    let mut seen_subpkg: HashSet<String> = HashSet::new();
    // Two passes over the same key set:
    //   1. Spot direct subdirs (those that contain a `package.mo`
    //      one segment deeper) and direct `.mo` files.
    //   2. Inline-child harvest from this package's own `package.mo`
    //      (handled below, after the main loop).
    for (path, _) in in_mem.files.iter() {
        let Some(rel) = path.to_str().and_then(|s| s.strip_prefix(&prefix_with_slash)) else {
            // Allow the bundle's root scan (`prefix == ""`) — every
            // key strips trivially.
            if prefix_with_slash.is_empty() {
                if let Some(s) = path.to_str() {
                    s
                } else {
                    continue;
                };
                // Fall through; handled by the explicit loop below.
                // (Kept simple — root scan rarely runs.)
            }
            continue;
        };
        // First segment after the prefix. Exactly one segment → leaf
        // file or top-level marker (`package.mo`); two or more → a
        // sub-package (we record the first segment).
        let mut segs = rel.split('/');
        let first = match segs.next() {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let is_deeper = segs.next().is_some();
        if is_deeper {
            // Sub-package candidate. Only count it if the bundle
            // actually has `<prefix>/<first>/package.mo` — that's the
            // marker that distinguishes a package directory from a
            // stray nested file (rare in MSL but possible).
            if seen_subpkg.contains(first) {
                continue;
            }
            let pkg_key = if prefix.is_empty() {
                format!("{first}/package.mo")
            } else {
                format!("{prefix}/{first}/package.mo")
            };
            if in_mem.files.contains_key(&PathBuf::from(&pkg_key)) {
                seen_subpkg.insert(first.to_string());
                let sub_path = if package_path.is_empty() {
                    first.to_string()
                } else {
                    format!("{package_path}.{first}")
                };
                let id = format!("msl_{}", sub_path.replace('.', "_"));
                results.push(PackageNode::Category {
                    id,
                    name: first.to_string(),
                    package_path: sub_path,
                    fs_path: PathBuf::from(&pkg_key),
                    children: None,
                    is_loading: false,
                });
            }
        } else if first.ends_with(".mo") && first != "package.mo" {
            let display_name = first.trim_end_matches(".mo").to_string();
            let qualified = if package_path.is_empty() {
                display_name.clone()
            } else {
                format!("{package_path}.{display_name}")
            };
            let key = if prefix.is_empty() {
                first.to_string()
            } else {
                format!("{prefix}/{first}")
            };
            // Multi-class package files (e.g.
            // `Modelica/Blocks/Continuous.mo` containing
            // `package Continuous ... block Integrator ... block
            // Derivative ...`) need to render as expandable tree
            // nodes, not opaque leaves. Use the pre-parsed AST from
            // the MSL bundle (cheap dictionary lookup; no parse on
            // the main thread) and, if the top class is a package
            // with children, emit a Category whose children are the
            // inner classes. Falls back to a flat leaf when the file
            // holds a single class (the common MSL leaf shape) or
            // when no parsed AST is available yet.
            if let Some(node) = parsed_msl_to_package_node(&key, &qualified, &display_name) {
                results.push(node);
            } else {
                let kind_str = in_mem
                    .files
                    .get(&PathBuf::from(&key))
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .and_then(peek_class_kind_from_source);
                results.push(leaf_model_node(&qualified, &display_name, kind_str));
            }
        }
    }

    // Inline children harvest from `<prefix>/package.mo` — same as
    // the native path, but reading from in-memory bundle.
    let pkg_key = if prefix.is_empty() {
        "package.mo".to_string()
    } else {
        format!("{prefix}/package.mo")
    };
    if let Some(bytes) = in_mem.files.get(&PathBuf::from(&pkg_key)) {
        if let Ok(source_str) = std::str::from_utf8(bytes) {
            let ast = rumoca_phase_parse::parse_to_recovered_ast(source_str, &pkg_key);
            if let Some((_, top_class)) = ast.classes.iter().next() {
                let existing_names: HashSet<String> =
                    results.iter().map(|n| n.name().to_string()).collect();
                for (child_short, child_def) in &top_class.classes {
                    if existing_names.contains(child_short) {
                        continue;
                    }
                    let child_qualified = if package_path.is_empty() {
                        child_short.to_string()
                    } else {
                        format!("{package_path}.{child_short}")
                    };
                    results.push(class_def_to_node(
                        &PathBuf::from(&pkg_key),
                        &child_qualified,
                        child_short,
                        child_def,
                    ));
                }
            }
        }
    }

    results.sort_by_key(omedit_sort_key);
    results
}

/// Build a `PackageNode` for a multi-class MSL file by reading its
/// pre-parsed AST out of [`crate::msl_remote::global_parsed_msl`].
/// Returns `None` when no parsed AST is available (cold boot before
/// MSL bundle ready, or a file that isn't part of the bundle), when
/// the source is a single-class leaf (the common MSL shape — caller
/// emits a flat leaf in that case), or when the top class has no
/// inner classes (e.g. an empty package). Wasm-only — native takes
/// the disk-walking path.
#[cfg(target_arch = "wasm32")]
fn parsed_msl_to_package_node(
    bundle_key: &str,
    qualified: &str,
    short_name: &str,
) -> Option<PackageNode> {
    use rumoca_session::parsing::ClassType;
    use std::path::PathBuf;
    let bundle = crate::msl_remote::global_parsed_msl()?;
    let ast = bundle.iter().find(|(k, _)| k == bundle_key).map(|(_, a)| a)?;
    let (_top_name, top_class) = ast.classes.iter().next()?;
    if !matches!(top_class.class_type, ClassType::Package) || top_class.classes.is_empty() {
        return None;
    }
    let bundle_path = PathBuf::from(bundle_key);
    Some(class_def_to_node(&bundle_path, qualified, short_name, top_class))
}

/// Cheap class-kind sniffer for leaf MSL files. Returns `Some("model")`
/// / `"connector"` / `"package"` / etc. by scanning the first
/// non-comment, non-encapsulated keyword in the source. Native uses
/// `peek_class_header` which calls into the rumoca lexer and also
/// reads the file from disk; this duplicate avoids the read step on
/// wasm where the source is already in memory.
#[cfg(target_arch = "wasm32")]
fn peek_class_kind_from_source(src: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with("/*") {
            continue;
        }
        for word in line.split_whitespace() {
            match word {
                "model" | "block" | "connector" | "package"
                | "record" | "type" | "function" | "class" => {
                    return Some(word.to_string());
                }
                "encapsulated" | "partial" | "operator" | "expandable"
                | "pure" | "impure" | "redeclare" | "final"
                | "inner" | "outer" | "replaceable" => continue,
                _ => return None,
            }
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_msl_dir_native(dir: &std::path::Path, package_path: String) -> Vec<PackageNode> {
    let mut results = Vec::new();

    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                if name.starts_with('.') || name == "__MACOSX" { continue; }
                let sub_path = format!("{}.{}", package_path, name);
                let id = format!("msl_{}", sub_path.replace('.', "_").replace('/', "_"));
                results.push(PackageNode::Category {
                    id,
                    name,
                    package_path: sub_path,
                    fs_path: path,
                    children: None, // Lazy load
                    is_loading: false,
                });
            } else if path.extension().map(|e| e == "mo").unwrap_or(false) {
                if name == "package.mo" {
                    // Don't surface `package.mo` as its own row — it
                    // IS the parent package's definition. But its
                    // inline sub-classes (e.g. `package Examples` in
                    // `Blocks/package.mo`) are real siblings of the
                    // other `.mo` files in this directory; harvest
                    // them below outside the per-entry loop.
                    continue;
                }
                let display_name = name.strip_suffix(".mo").unwrap_or(&name).to_string();
                let qualified = format!("{}.{}", package_path, display_name);
                results.push(node_from_modelica_file(&path, &qualified, &display_name));
            }
        }
    }

    // Merge inline children from `package.mo`. MSL packages are
    // hybrid: some children are sibling `.mo` files (Continuous.mo,
    // Discrete.mo), others live inline inside `package.mo`
    // (Examples, Noise, BusUsage_Utilities, ...). OMEdit shows them
    // all as peers of the parent — we need to do the same or
    // Examples is invisible. Skip duplicates: if a sibling file
    // already provides a class of that name, keep the file version.
    let pkg_mo = dir.join("package.mo");
    if pkg_mo.is_file() {
        if let Ok(source) = std::fs::read_to_string(&pkg_mo) {
            let ast = rumoca_phase_parse::parse_to_recovered_ast(
                &source,
                &pkg_mo.display().to_string(),
            );
            if let Some((_, top_class)) = ast.classes.iter().next() {
                let existing_names: std::collections::HashSet<String> =
                    results.iter().map(|n| n.name().to_string()).collect();
                for (child_short, child_def) in &top_class.classes {
                    if existing_names.contains(child_short) {
                        continue;
                    }
                    let child_qualified = format!("{}.{}", package_path, child_short);
                    results.push(class_def_to_node(
                        &pkg_mo,
                        &child_qualified,
                        child_short,
                        child_def,
                    ));
                }
            }
        }
    }

    results.sort_by_key(omedit_sort_key);
    results
}

/// Top-level grouping in the OMEdit-style browser. Variants are
/// declared in display order, so `derive(Ord)` gives the correct
/// sort priority.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum SortGroup {
    UsersGuide,
    Examples,
    SubPackage,
    Leaf(LeafKind),
}

/// Sub-grouping for leaf classes. Order = sort priority. `Other`
/// catches unknown kinds so new Modelica keywords don't silently
/// reshuffle the tree.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum LeafKind {
    Model,
    Block,
    Connector,
    Record,
    Function,
    Type,
    Constant,
    Other,
}

impl LeafKind {
    fn from_str(kind: Option<&str>) -> Self {
        match kind {
            Some("model") => Self::Model,
            Some("block") => Self::Block,
            Some("connector") => Self::Connector,
            Some("record") => Self::Record,
            Some("function") => Self::Function,
            Some("type") => Self::Type,
            Some("constant") => Self::Constant,
            _ => Self::Other,
        }
    }
}

/// Sort key matching OMEdit's tree convention. Priority order:
///
/// 1. `UsersGuide` — documentation hub, always pinned to the top of
///    its parent (every MSL top-level package has one).
/// 2. `Examples` — runnable example collection, second pin.
/// 3. Other sub-packages (folders) — alphabetical.
/// 4. Leaf classes — grouped by class kind (model → block →
///    connector → record → function → type → constant → other),
///    then alphabetical within each group. Reading by kind is how
///    OMEdit-style browsers surface large packages: all the
///    instantiable models cluster together, supporting types follow.
///
/// Both the disk scan and the inline AST walk use this same key, so
/// directory- and single-file-packaged content sort identically.
fn omedit_sort_key(n: &PackageNode) -> (SortGroup, String) {
    let group = match n.name() {
        "UsersGuide" => SortGroup::UsersGuide,
        "Examples" => SortGroup::Examples,
        _ => match n {
            PackageNode::Category { .. } => SortGroup::SubPackage,
            PackageNode::Model { class_kind, .. } => {
                SortGroup::Leaf(LeafKind::from_str(class_kind.as_deref()))
            }
        },
    };
    (group, n.name().to_lowercase())
}

/// Scan the `lunco-assets` cache for unpacked Modelica libraries.
///
/// Returns `(cache_subdir, package_name)` for every top-level cache
/// entry whose unpacked archive contains a `<PackageName>/package.mo`
/// — Modelica's canonical structured-library marker (MLS §13.2.2.2).
/// `msl` is excluded (the MSL pipeline owns it) so the bundled
/// "LunCo Examples" row stays the last library entry by construction.
///
/// Adding a library is therefore data-only: drop a new `Assets.toml`
/// entry, run `cargo run -p lunco-assets -- download`, and the row
/// appears next launch — no edit here.
///
/// Output is sorted alphabetically by package name so the registration
/// order is deterministic across runs.
pub fn discover_third_party_libs() -> Vec<(String, String)> {
    let cache = lunco_assets::cache_dir();
    let Ok(entries) = std::fs::read_dir(&cache) else { return Vec::new(); };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let subdir = entry.file_name().to_string_lossy().into_owned();
        if subdir == "msl" || subdir.starts_with('.') {
            continue;
        }
        // First (and typically only) inner directory containing a
        // `package.mo` is the unpacked Modelica library root. Other
        // siblings (READMEs, LICENSE, examples folders) are ignored.
        let Ok(inner) = std::fs::read_dir(&path) else { continue; };
        for inner_entry in inner.flatten() {
            let inner_path = inner_entry.path();
            if inner_path.is_dir() && inner_path.join("package.mo").is_file() {
                let pkg = inner_entry.file_name().to_string_lossy().into_owned();
                out.push((subdir.clone(), pkg));
                break;
            }
        }
    }
    out.sort();
    out
}

fn build_bundled_tree() -> Vec<PackageNode> {
    use crate::visual_diagram::{msl_bundled_trees, BundledClassTree};
    // Pre-baked tree shape from `msl_indexer` (in `msl_index.json`).
    // Lets multi-class bundled files render with proper kind badges
    // and expandable inner classes — same shape MSL files get from
    // the disk scan. `bundled_models()` still owns the source text;
    // the index just adds the structure we'd otherwise have to
    // parse at startup. Keyed by filename.
    let trees = msl_bundled_trees();
    let trees_by_filename: std::collections::HashMap<&str, &BundledClassTree> =
        trees.iter().map(|t| (t.filename.as_str(), &t.top)).collect();
    bundled_models()
        .iter()
        .map(|m| match trees_by_filename.get(m.filename) {
            Some(tree) => bundled_class_to_node(m.filename, tree),
            // No index entry (legacy `msl_index.json` predating the
            // bundled-tree extension, or a `.mo` added since the
            // last indexer run) — fall back to a flat leaf with a
            // `model` badge guess. Re-running `msl_indexer` upgrades
            // it to the proper shape next launch.
            None => PackageNode::Model {
                id: format!("bundled://{}", m.filename),
                name: m
                    .filename
                    .strip_suffix(".mo")
                    .unwrap_or(m.filename)
                    .to_string(),
                library: ModelLibrary::Bundled,
                class_kind: Some("model".to_string()),
            },
        })
        .collect()
}

/// Convert one indexer-baked [`BundledClassTree`] into a
/// [`PackageNode`]. Package classes become Categories with
/// recursively-built children; leaves become Models. Click ids
/// route through `bundled://<filename>#<qualified>` so the open
/// handler can drill into a specific inner class — matches MSL's
/// `msl_path:` drill-in convention but scoped to the bundled
/// source file.
fn bundled_class_to_node(
    filename: &str,
    tree: &crate::visual_diagram::BundledClassTree,
) -> PackageNode {
    let is_package = tree.class_kind == "package";
    if is_package && !tree.children.is_empty() {
        let mut children: Vec<PackageNode> = tree
            .children
            .iter()
            .map(|child| bundled_class_to_node(filename, child))
            .collect();
        children.sort_by_key(omedit_sort_key);
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
            class_kind: Some(tree.class_kind.clone()),
        }
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

/// System that checks for finished scanning tasks and updates the cache.
pub fn handle_package_loading_tasks(
    mut cache: ResMut<PackageTreeCache>,
    mut workbench: ResMut<WorkbenchState>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut model_tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut layout: ResMut<lunco_workbench::WorkbenchLayout>,
    mut egui_ctx: bevy_egui::EguiContexts,
    mut pending_drill_ins: ResMut<crate::ui::browser_dispatch::PendingDrillIns>,
    mut drilled_in: ResMut<crate::ui::panels::canvas_diagram::DrilledInClassNames>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
) {
    let mut finished_results = Vec::new();

    cache.tasks.retain_mut(|task| {
        if let Some(result) = future::block_on(future::poll_once(task)) {
            finished_results.push(result);
            false // Remove task
        } else {
            true // Keep task
        }
    });

    for result in finished_results {
        find_and_update_node(&mut cache.roots, &result.parent_id, result.children);
    }

    // Bundled tree rebuild on wasm. The eager build at
    // `PackageTreeCache::new()` ran before the MSL bundle finished
    // fetching, so every file showed as a flat leaf instead of an
    // expandable package. As soon as the indexer's per-file class
    // trees become available (i.e. after `MslRemotePlugin` installs
    // the `MslAssetSource::InMemory`), rebuild the `bundled_root`
    // children once so LunCo Examples / future bundled libraries
    // expose their inner classes (Engine, Tank, RocketStage, …).
    if !cache.bundled_tree_indexed
        && !crate::visual_diagram::msl_bundled_trees().is_empty()
    {
        let new_children = build_bundled_tree();
        for root in cache.roots.iter_mut() {
            if let PackageNode::Category { id, children, .. } = root {
                if id == "bundled_root" {
                    *children = Some(new_children);
                    break;
                }
            }
        }
        cache.bundled_tree_indexed = true;
    }

    // Process file loading tasks
    let mut finished_files = Vec::new();
    cache.file_tasks.retain_mut(|task| {
        if let Some(result) = future::block_on(future::poll_once(task)) {
            finished_files.push(result);
            false
        } else {
            true
        }
    });

    // Poll any in-flight Twin folder scan. When it finishes, install
    // the scanned tree into `cache.twin`. Keeps the spinner up while
    // pending; drops it to `None` when done.
    if let Some(mut task) = cache.twin_scan_task.take() {
        if let Some(scanned) = future::block_on(future::poll_once(&mut task)) {
            cache.twin = Some(scanned);
            cache.twin_scan_task = None;
        } else {
            cache.twin_scan_task = Some(task);
        }
    }

    for result in finished_files {
        // Drop the dedup guard: subsequent clicks on the same row
        // now go through the registry-lookup branch and re-focus
        // the existing tab instead of spawning another parse.
        cache.loading_ids.remove(&result.id);
        // Final font-dependent shaping on main thread
        let cached_galley = result.layout_job.map(|job| {
            egui_ctx.ctx_mut().unwrap().fonts_mut(|f| f.layout_job(job))
        });

        // Document was pre-allocated off-thread by the bg task —
        // installing here is just a HashMap insert. The expensive
        // rumoca parse already finished by the time this runs, so
        // the UI never blocks on it.
        let doc_id = result.doc_id;
        registry.install_prebuilt(doc_id, result.doc);

        // If the Twin Browser dispatcher queued a drill-in for this
        // file, apply it now. The canvas projector reads
        // `DrilledInClassNames` on its next tick and lands on the
        // requested class — saves a second click.
        let queued_qualified = pending_drill_ins.take(&result.id);
        if let Some(qualified) = queued_qualified {
            drilled_in.set(doc_id, qualified);
        }

        workbench.open_model = Some(OpenModel {
            model_path: result.id,
            display_name: result.name,
            source: result.source,
            line_starts: result.line_starts,
            detected_name: result.detected_name,
            cached_galley,
            read_only: result.library != ModelLibrary::InMemory
                && result.library != ModelLibrary::User,
            library: result.library,
        });
        workbench.diagram_dirty = true;
        workbench.is_loading = false;
        // Sync active document into the Workspace session.
        workspace.active_document = Some(doc_id);

        // Open (or focus) the multi-instance tab for this document.
        model_tabs.ensure(doc_id);
        layout.open_instance(
            crate::ui::panels::model_view::MODEL_VIEW_KIND,
            doc_id.raw(),
        );
    }
}

// ---------------------------------------------------------------------------
// Package Browser Panel
// ---------------------------------------------------------------------------

pub struct PackageBrowserPanel;

impl Panel for PackageBrowserPanel {
    fn id(&self) -> PanelId { PanelId("modelica_package_browser") }
    fn title(&self) -> String { "📚 Package Browser".into() }
    fn default_slot(&self) -> PanelSlot { PanelSlot::SideBrowser }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        // Expand Bundled by default (first run)
        ui.memory_mut(|m| {
            if m.data.get_temp::<bool>(egui::Id::new("tree_expand_bundled_root")).is_none() {
                m.data.insert_temp(egui::Id::new("tree_expand_bundled_root"), true);
            }
        });


        // Fetch needed state from World before borrowing tree_cache mutably
        let active_path_str = {
            let state = world.resource::<WorkbenchState>();
            state.open_model.as_ref().map(|m| m.model_path.clone())
        };
        let active_path = active_path_str.as_deref();
        let muted = world
            .get_resource::<lunco_theme::Theme>()
            .map(|t| t.tokens.text_subdued)
            .unwrap_or(egui::Color32::DARK_GRAY);
        let to_open: Option<PackageAction> = None;
        let mut reopen_in_memory: Option<String> = None;
        let mut create_new = false;
        let mut open_twin_picker = false;
        let mut close_twin = false;
        let mut open_twin_file: Option<std::path::PathBuf> = None;
        let mut pending_rename: Option<(std::path::PathBuf, std::path::PathBuf)> = None;

        {
            let mut tree_cache = world.resource_mut::<PackageTreeCache>();

            // `auto_shrink([false; 2])` tells egui to fill the full
            // panel rect regardless of content size — without it the
            // scroll viewport can end up shorter than the panel
            // height, cutting off the last items and giving users no
            // way to scroll to them (the symptom you hit with long
            // package trees).
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                // Clamp every descendant label to the panel width
                // and truncate with ellipsis if it doesn't fit.
                // Without this, long names (deep MSL paths, rename
                // buffers, workspace paths) spill past the panel
                // edge and the leading characters end up hidden
                // behind neighbouring UI.
                ui.set_max_width(ui.available_width());
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                let cache = &mut *tree_cache;

                // ── WORKSPACE ──
                // One unified section. Header shows the open Twin
                // folder's name (or "No folder") and exposes the
                // open/close action. Inside, an always-visible
                // (Untitled) virtual group gathers scratch models
                // that aren't yet bound to a path — matches VS Code's
                // handling of untitled buffers in Explorer.
                let twin_label = if let Some(twin) = cache.twin.as_ref() {
                    twin.root
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| twin.root.display().to_string())
                } else {
                    "No folder".to_string()
                };
                section_header(ui, &twin_label, |ui| {
                    // ➕ New is always here (VS Code parity — you can
                    // always make a scratch model).
                    if ui
                        .small_button("➕")
                        .on_hover_text("New model (Ctrl+N)")
                        .clicked()
                    {
                        create_new = true;
                    }
                    if cache.twin_scan_task.is_some() {
                        ui.spinner();
                    } else if cache.twin.is_some() {
                        if ui
                            .small_button("✕")
                            .on_hover_text("Close folder")
                            .clicked()
                        {
                            close_twin = true;
                        }
                    } else if ui
                        .small_button("📁")
                        .on_hover_text("Open a folder")
                        .clicked()
                    {
                        open_twin_picker = true;
                    }
                });

                // Untitled virtual folder — only rendered when there's
                // at least one scratch model. Always top of the tree
                // so recently-created items stay visible.
                if !cache.in_memory_models.is_empty() {
                    let resp = egui::CollapsingHeader::new(
                        egui::RichText::new("(Untitled)")
                            .size(11.0)
                            .italics()
                            .color(egui::Color32::from_rgb(220, 220, 160)),
                    )
                    .id_salt("workspace_untitled")
                    .default_open(true)
                    .show(ui, |ui| {
                        if let Some(id) = render_in_memory_models(
                            ui,
                            &cache.in_memory_models,
                            active_path,
                        ) {
                            reopen_in_memory = Some(id);
                        }
                    });
                    resp.header_response
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                }

                // Twin folder (if any).
                if cache.twin_scan_task.is_some() {
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("Scanning folder…")
                                .size(11.0)
                                .color(egui::Color32::GRAY),
                        );
                    });
                } else if let Some(twin) = cache.twin.clone() {
                    if twin.root_node.children.is_empty() {
                        section_empty_state(
                            ui,
                            "Empty folder. Add a .mo file on disk and reopen.",
                        );
                    } else {
                        // `cache.rename` is borrowed mutably into
                        // every node render so the active rename row
                        // can own the TextEdit buffer. `twin` is
                        // cloned above to avoid aliasing.
                        for child in &twin.root_node.children {
                            let action = render_twin_node(ui, child, &mut cache.rename);
                            if let Some(path) = action.open {
                                open_twin_file = Some(path);
                            }
                            if let Some(path) = action.rename {
                                cache.rename.target = Some(path.clone());
                                cache.rename.buffer = path
                                    .file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                cache.rename.needs_focus = true;
                            }
                            if let Some((from, to)) = action.commit_rename {
                                pending_rename = Some((from, to));
                            }
                            if action.cancel_rename {
                                cache.rename = RenameState::default();
                            }
                        }
                    }
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(twin.root.display().to_string())
                            .size(9.0)
                            .color(muted),
                    );
                } else if cache.in_memory_models.is_empty() {
                    // No Twin AND no scratch — show the real empty
                    // state so the sidebar isn't a blank rectangle.
                    section_empty_state(
                        ui,
                        "No work open. Open a folder, or ➕ for a new model.",
                    );
                }
            });
        }

        if create_new {
            // VS Code-style: one click → new Untitled tab immediately.
            // The observer in `ui::commands` picks a unique
            // `Untitled<N>` name, allocates the doc, and opens a tab.
            world.commands().trigger(crate::ui::commands::CreateNewScratchModel {});
        }

        // ── Twin lifecycle ──────────────────────────────────────
        if open_twin_picker {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(folder) = rfd::FileDialog::new()
                .set_title("Open Twin folder")
                .pick_folder()
            {
                if let Some(mut console) = world
                    .get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
                {
                    console.info(format!("Scanning twin folder: {}", folder.display()));
                }
                let pool = AsyncComputeTaskPool::get();
                let task = pool.spawn(async move { scan_twin_folder(folder) });
                let mut cache = world.resource_mut::<PackageTreeCache>();
                cache.twin = None; // clear old tree so spinner shows
                cache.twin_scan_task = Some(task);
            }
        }
        if close_twin {
            let mut cache = world.resource_mut::<PackageTreeCache>();
            cache.twin = None;
            cache.twin_scan_task = None;
        }
        if let Some(path) = open_twin_file {
            // Treat the clicked .mo as a user-writable file. Use the
            // existing disk-load path so loading + tab-open flows are
            // consistent with clicks on Examples (minus writability).
            let id = path.to_string_lossy().into_owned();
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| id.clone());
            open_model(world, id, name, ModelLibrary::User);
        }

        // Commit a rename — std::fs::rename, then trigger a rescan so
        // the tree reflects the new name. If rename fails (conflict,
        // permissions) log and leave state unchanged so the user can
        // retry or cancel.
        if let Some((from, to)) = pending_rename {
            if to.exists() {
                let msg = format!(
                    "Rename cancelled: '{}' already exists.",
                    to.display()
                );
                log::warn!("[Rename] {msg}");
                if let Some(mut console) = world
                    .get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
                {
                    console.warn(msg);
                }
            } else if let Err(e) = std::fs::rename(&from, &to) {
                let msg = format!(
                    "Rename failed: {} -> {}: {e}",
                    from.display(),
                    to.display()
                );
                log::error!("[Rename] {msg}");
                if let Some(mut console) = world
                    .get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
                {
                    console.error(msg);
                }
            } else {
                let msg = format!("Renamed {} -> {}", from.display(), to.display());
                log::info!("[Rename] {msg}");
                if let Some(mut console) = world
                    .get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
                {
                    console.info(msg);
                }
                // Re-scan the twin to pick up the new name. Same
                // async path as Open Folder.
                if let Some(root) = world
                    .resource::<PackageTreeCache>()
                    .twin
                    .as_ref()
                    .map(|t| t.root.clone())
                {
                    use bevy::tasks::AsyncComputeTaskPool;
                    let pool = AsyncComputeTaskPool::get();
                    let task = pool.spawn(async move { scan_twin_folder(root) });
                    let mut cache = world.resource_mut::<PackageTreeCache>();
                    cache.twin_scan_task = Some(task);
                }
            }
            world.resource_mut::<PackageTreeCache>().rename = RenameState::default();
        }

        if let Some(action) = to_open {
            match action {
                PackageAction::Open(id, name, lib) => {
                    queue_drill_in_if_inline(world, &id, &lib);
                    open_model(world, id, name, lib);
                }
                PackageAction::Instantiate { msl_path, display_name } => {
                    instantiate_on_active_canvas(world, &msl_path, &display_name);
                }
                PackageAction::DragStart { msl_path } => stash_drag_payload(world, &msl_path),
            }
        }

        // Re-open an already-allocated in-memory model. We pass the id;
        // `open_model`'s mem:// branch now consults the registry to
        // restore the user's current source rather than regenerating
        // from a template.
        if let Some(id) = reopen_in_memory {
            // Name is the part after "mem://".
            let name = id.strip_prefix("mem://").unwrap_or(&id).to_string();
            open_model(world, id, name, ModelLibrary::InMemory);
        }
    }
}

enum PackageAction {
    Open(String, String, ModelLibrary),
    /// Double-click on a class row — instantiate it on the currently
    /// active canvas tab. Routes through the same `AddModelicaComponent`
    /// Reflect event the palette uses, so origin-level read-only
    /// rejection applies uniformly.
    Instantiate { msl_path: String, display_name: String },
    /// User started dragging a class row — stash a
    /// [`ComponentDragPayload`] (same resource the palette uses) so
    /// the canvas's drop handler picks it up on pointer release.
    /// `msl_path` resolves to a `MSLComponentDef` via
    /// [`crate::visual_diagram::msl_component_by_path`]; payload is a
    /// no-op when the path isn't in the static MSL library (e.g.
    /// Bundled-only entries — drag falls back to double-click).
    DragStart { msl_path: String },
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_node(
    node: &mut PackageNode,
    ui: &mut egui::Ui,
    active_path: Option<&str>,
    active_drill: Option<&str>,
    depth: usize,
    tasks: &mut Vec<Task<ScanResult>>,
    theme: &lunco_theme::Theme,
) -> Option<PackageAction> {
    // Compare a tree-row id to the active document + drill-in state.
    // Bundled rows use `bundled://<file>#<qualified>`, so the file
    // half must match the active doc's `model_path` (with the
    // fragment stripped on open) AND the qualified half must match
    // the canvas's drill-in target. Non-bundled ids fall back to
    // the bare file-path equality the legacy comparison did.
    let id_is_active = |id: &str| -> bool {
        if let Some((file, qual)) = id.split_once('#') {
            active_path == Some(file) && active_drill == Some(qual)
        } else {
            active_path == Some(id) && active_drill.is_none()
        }
    };
    let indent = depth as f32 * 16.0 + 4.0;
    let mut result = None;

    match node {
        PackageNode::Category { id, name, children, is_loading, fs_path, package_path } => {
            let expand_id = egui::Id::new(format!("tree_expand_{}", id));
            let is_expanded = ui.memory(|m| m.data.get_temp::<bool>(expand_id).unwrap_or(false));
            let arrow = if is_expanded { "▼" } else { "▶" };
            // Highlight the row when its id matches the active
            // doc + drill-in pair. Currently meaningful for bundled
            // package rows whose `bundled://<file>#<qualified>` id
            // can refer to a `package` the user is currently
            // viewing. Non-bundled Categories are directories with
            // no doc-path equivalent, so they never match — same as
            // before, no regression.
            let is_active = id_is_active(id);
            let bg_active = if is_active {
                egui::Color32::from_rgba_unmultiplied(80, 80, 0, 40)
            } else {
                egui::Color32::TRANSPARENT
            };

            let resp = ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.add_sized([12.0, 12.0], egui::Label::new(
                    egui::RichText::new(arrow).size(8.0).color(egui::Color32::GRAY)
                ).selectable(false));
                ui.add(egui::Label::new(
                    egui::RichText::new(name.as_str()).size(11.0)
                ).selectable(false).sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
            }).inner;
            if is_active {
                ui.painter().rect_filled(resp.rect, 2.0, bg_active);
            }

            if resp.clicked() {
                ui.memory_mut(|m| m.data.insert_temp(expand_id, !is_expanded));
            }

            if is_expanded {
                if let Some(ref mut children_vec) = children {
                    let limit_id = egui::Id::new(format!("tree_limit_{}", id));
                    let limit = ui.memory(|m| m.data.get_temp::<usize>(limit_id).unwrap_or(100));

                    for (idx, child) in children_vec.iter_mut().enumerate() {
                        if idx >= limit {
                            ui.horizontal(|ui| {
                                ui.add_space(indent + 16.0);
                                if ui.button(format!("... and {} more (click to show all)", children_vec.len() - limit)).clicked() {
                                    ui.memory_mut(|m| m.data.insert_temp(limit_id, children_vec.len()));
                                }
                            });
                            break;
                        }
                        if let Some(req) = render_node(
                            child,
                            ui,
                            active_path,
                            active_drill,
                            depth + 1,
                            tasks,
                            theme,
                        ) {
                            result = Some(req);
                        }
                    }
                } else if !*is_loading {
                    // Trigger load
                    *is_loading = true;
                    let pool = AsyncComputeTaskPool::get();
                    let parent_id = id.clone();
                    let scan_dir = fs_path.clone();
                    let pkg_path = package_path.clone();

                    let task = pool.spawn(async move {
                        let children = scan_msl_dir(&scan_dir, pkg_path);
                        ScanResult { parent_id, children }
                    });
                    tasks.push(task);
                }

                if *is_loading {
                    ui.horizontal(|ui| {
                        ui.add_space(indent + 16.0);
                        ui.label(egui::RichText::new("⌛ Loading...").size(10.0).italics().color(egui::Color32::GRAY));
                    });
                }
            }
        }

        PackageNode::Model {
            id,
            name,
            library,
            class_kind,
        } => {
            let is_active = id_is_active(id);

            let bg = if is_active {
                egui::Color32::from_rgba_unmultiplied(80, 80, 0, 40)
            } else {
                egui::Color32::TRANSPARENT
            };

            // Class-kind awareness via the static MSL library — the
            // same `MSLComponentDef` the palette consumes. When the
            // path resolves, render a Modelica-class badge (M/B/X/F/R/...)
            // matching the workspace section's style, and gate drag
            // on `is_instantiable` so connectors / functions /
            // partial-interfaces don't drag onto a diagram (you
            // reference them in source instead — Dymola/OMEdit
            // behaviour). Bundled / user / in-memory rows fall back
            // to the source-library icon and click-only.
            let msl_path = msl_path_for_id(id, library);
            let def = matches!(library, ModelLibrary::MSL)
                .then(|| crate::visual_diagram::msl_component_by_path(&msl_path))
                .flatten();
            // Prefer the static `MSLComponentDef.class_kind` (richer
            // — also gives `is_instantiable`'s heuristics for
            // .Interfaces / .Internal); fall back to the kind peeked
            // at scan time so classes the static index doesn't cover
            // (records, types, constants, partial-model siblings)
            // still get the right badge.
            let resolved_kind: Option<&str> = def
                .as_ref()
                .map(|d| d.class_kind.as_str())
                .or_else(|| class_kind.as_deref());
            let drag_enabled = match (&def, resolved_kind) {
                (Some(d), _) => crate::ui::panels::palette::is_instantiable(d),
                // No static def → drag if the peeked kind is
                // instantiable on a diagram. Mirrors the same
                // model/block whitelist `is_instantiable` uses,
                // minus the .Interfaces / .Internal path filters
                // (those need the def to disambiguate).
                (None, Some(k)) => matches!(k, "model" | "block"),
                (None, None) => false,
            };
            let sense = if drag_enabled {
                egui::Sense::click_and_drag()
            } else {
                egui::Sense::click()
            };

            // Use the workspace section's exact `paint_badge` helper
            // when we have a kind — same coloured pill, same letter
            // mapping. Falls back to a source-library emoji only when
            // we have no kind at all (rare; mostly Bundled today
            // before the bundled-metadata index lands).
            let resp = ui
                .horizontal(|ui| {
                    ui.add_space(indent + 16.0);
                    if let Some(k) = resolved_kind {
                        let badge = crate::ui::browser_section::type_badge_from_str(k, theme);
                        crate::ui::browser_section::paint_badge(ui, badge, theme);
                    } else {
                        let icon = match library {
                            ModelLibrary::MSL => "?",
                            ModelLibrary::Bundled => "📦",
                            ModelLibrary::User => "📁",
                            ModelLibrary::InMemory => "💾",
                        };
                        ui.label(egui::RichText::new(icon).size(11.0));
                    }
                    ui.add(
                        egui::Label::new(egui::RichText::new(name.as_str()).size(11.0))
                            .sense(sense),
                    )
                })
                .inner;

            if is_active {
                ui.painter().rect_filled(resp.rect, 2.0, bg);
            }

            // Interaction semantics, mirroring the Component Palette
            // so the tree and the palette behave identically:
            //  - drag → stash a `ComponentDragPayload`; the canvas
            //    drop handler reads it on pointer release and places
            //    the class at the cursor.
            //  - single click → open as a (read-only) tab. Drill-in /
            //    inspection use case.
            //  - double-click → instantiate at the canvas grid origin
            //    (same path as the palette's click-to-add). Routes
            //    through the same `AddModelicaComponent` Reflect
            //    event the palette uses, so document-layer read-only
            //    enforcement applies uniformly.
            //
            // Note: egui fires `clicked()` on the first release of a
            // double-click too, so a double-click also opens a tab —
            // accepted behaviour ("double-click opens the source AND
            // adds the instance"); users who want only one can use
            // single click vs the canvas's right-click menu.
            if resp.drag_started() {
                result = Some(PackageAction::DragStart {
                    msl_path: msl_path_for_id(id, library),
                });
            } else if resp.double_clicked() {
                // `id` shape decides Open vs Instantiate:
                //   * `bundled://X.mo`            → top-level file
                //                                    row → Open as a
                //                                    new tab.
                //   * `bundled://X.mo#Class.Sub`  → nested-class row
                //                                    → instantiate
                //                                    into the active
                //                                    doc's canvas
                //                                    (palette
                //                                    parity).
                //
                // Previous behaviour always emitted `Instantiate`,
                // which silently no-op'd whenever the active-doc
                // tracker was None — every other example open after
                // a tab-close logged `[PackageBrowser] double-click
                // on … ignored — no active document` and dropped the
                // user's intent. Closing every tab and reopening
                // worked because that path re-bound active_doc.
                let is_nested = id.contains('#');
                if is_nested {
                    result = Some(PackageAction::Instantiate {
                        msl_path: msl_path_for_id(id, library),
                        display_name: name.clone(),
                    });
                } else {
                    result = Some(PackageAction::Open(
                        id.clone(),
                        name.clone(),
                        library.clone(),
                    ));
                }
            } else if resp.clicked() {
                result = Some(PackageAction::Open(
                    id.clone(),
                    name.clone(),
                    library.clone(),
                ));
            }

            // Hover stays lightweight — qualified path + library
            // tag, no docstring. The class's description belongs in
            // the Docs view (model_view's `render_docs_view`), which
            // renders it as the page subtitle and lays out the full
            // `Documentation(info=…)` HTML annotation underneath.
            // Mirroring the docstring on hover would just duplicate
            // info that's one click away, and the workspace section
            // already follows the same restraint.
            if resp.hovered() {
                let lib_tag: &str = match library {
                    ModelLibrary::MSL => "📚 MSL — read-only",
                    ModelLibrary::Bundled => "📦 Bundled — read-only",
                    ModelLibrary::User => "📁 User model — writable",
                    ModelLibrary::InMemory => "💾 In-memory — writable",
                };
                let qualified = msl_path.clone();
                let display_name = name.clone();
                resp.on_hover_ui(move |ui| {
                    ui.strong(display_name);
                    ui.label(egui::RichText::new(qualified).small().weak());
                    ui.label(egui::RichText::new(lib_tag).small().weak());
                });
            }
        }
    }

    result
}

/// Flush any in-progress edits on the currently-open model into its
/// Document so navigating away doesn't lose work.
///
/// Two paths are covered:
///
/// 1. **Text edits in the code editor** — the TextEdit focus-loss hook
///    already handles the common case, but a click in the Package
///    Browser doesn't always trigger `lost_focus()` on the editor. We
///    re-commit defensively from `EditorBufferState.text`.
/// 2. **Visual diagram edits** — `DiagramState.diagram` holds the
///    user's placed components / wires. If non-empty, regenerate
///    Modelica source and checkpoint it into the Document. This is the
///    diagram equivalent of focus-loss commit.
///
/// Both write through `ModelicaDocumentRegistry::checkpoint_source`,
/// which fires `DocumentChanged` so any subscriber (including the
/// re-open path via `find_by_path`) sees the fresh source.
///
/// No-op when the current model is read-only, has no backing Document,
/// or both buffers are empty.
fn commit_current_model_edits(world: &mut World) {
    // Snapshot everything we need up-front so we don't fight the borrow
    // checker when mutating the registry below. Active doc comes from
    // the Workspace session; the display-side snapshot (`read_only`,
    // `detected_name`) still lives on the `open_model` UI cache.
    let doc_id = match world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
    {
        Some(id) => id,
        None => return,
    };
    let (is_read_only, model_name) = {
        let state = world.resource::<WorkbenchState>();
        let Some(m) = state.open_model.as_ref() else { return };
        (
            m.read_only,
            m.detected_name
                .clone()
                .unwrap_or_else(|| m.display_name.clone()),
        )
    };
    if is_read_only {
        return;
    }

    // In Phase α, the diagram panel emits AST ops directly to the
    // document on every edit — the document is already the source of
    // truth for anything the user did in the diagram. No
    // regenerate-from-VisualDiagram checkpoint is needed (or correct —
    // the old path would overwrite hand-typed comments and
    // unrepresented annotations). We just commit the text-buffer
    // residue here: the code editor's focus-loss commit is normally
    // enough, but the user may switch panels before the widget has
    // fired `lost_focus()`, so we force a checkpoint on model switch.
    let _ = model_name; // kept above for future per-class targeting
    // Only commit when the editor buffer is bound to the *currently
    // active* doc. `EditorBufferState` is a singleton (one buffer
    // shared across every tab) — when the user switches tabs, the
    // buffer still holds the previous tab's text until `code_editor`
    // re-mirrors `open_model.source` into it on the next render.
    // Checkpointing without this guard pushes the previous tab's
    // bytes into the *new* active doc on every switch, bumping its
    // `generation` and triggering a spurious 5 s rumoca reparse on
    // wasm — the "switching back to AnnotatedRocketStage stalls"
    // symptom. `EditorBufferState.model_path` is updated to the
    // active model's path each time the editor mirrors the source;
    // if it matches `state.open_model.model_path` we know the buffer
    // genuinely belongs to this doc.
    let active_path = {
        let state = world.resource::<WorkbenchState>();
        state.open_model.as_ref().map(|m| m.model_path.clone())
    };
    let (buffer_path, buffer_text) = world
        .get_resource::<crate::ui::panels::code_editor::EditorBufferState>()
        .map(|b| (b.model_path.clone(), b.text.clone()))
        .unwrap_or_default();
    if active_path.as_deref() != Some(buffer_path.as_str()) {
        // Buffer hasn't been mirrored to the active doc yet — skip;
        // its contents belong to whichever tab the user just left.
        return;
    }
    if !buffer_text.is_empty() {
        world
            .resource_mut::<ModelicaDocumentRegistry>()
            .checkpoint_source(doc_id, buffer_text);
    }
}

/// Uniform section header for the sidebar. Label on the left in
/// muted caps, optional right-aligned action slot (e.g. `➕`, spinner,
/// close button). Matches VS Code Explorer's section-heading cadence.
fn section_header<F: FnOnce(&mut egui::Ui)>(
    ui: &mut egui::Ui,
    title: &str,
    right_actions: F,
) {
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(title.to_uppercase())
                .size(10.0)
                .color(egui::Color32::from_rgb(160, 160, 180))
                .strong(),
        );
        // Push actions to the right edge.
        let remaining = ui.available_width() - 60.0;
        if remaining > 0.0 {
            ui.add_space(remaining);
        }
        right_actions(ui);
    });
    ui.separator();
}

/// Muted placeholder text for empty sections. Keeps sections visually
/// present but non-noisy when there's nothing to show.
fn section_empty_state(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(text)
                .size(10.0)
                .italics()
                .color(egui::Color32::from_rgb(130, 130, 140)),
        );
    });
}

/// Signals that can come out of rendering a single tree node.
/// Returned from `render_twin_node`; the outer loop applies them
/// after the render pass so we don't mutate the cache while walking
/// the tree.
#[derive(Default)]
pub struct TwinNodeAction {
    /// User clicked a `.mo` file — open it as a tab.
    pub open: Option<std::path::PathBuf>,
    /// User invoked "Rename" from the context menu — enter rename
    /// mode for this path.
    pub rename: Option<std::path::PathBuf>,
    /// User pressed Enter in the rename TextEdit — commit rename
    /// from the first path to the second.
    pub commit_rename: Option<(std::path::PathBuf, std::path::PathBuf)>,
    /// User cancelled rename (Escape / blur).
    pub cancel_rename: bool,
}

impl TwinNodeAction {
    fn merge(&mut self, other: TwinNodeAction) {
        if other.open.is_some() {
            self.open = other.open;
        }
        if other.rename.is_some() {
            self.rename = other.rename;
        }
        if other.commit_rename.is_some() {
            self.commit_rename = other.commit_rename;
        }
        if other.cancel_rename {
            self.cancel_rename = true;
        }
    }
}

/// Render one Twin tree node. Directories use `CollapsingHeader` so
/// the twisty arrow / indentation / hover highlight come from egui
/// for free. Files are a selectable row with a right-click context
/// menu (Rename). Non-Modelica files render greyed + disabled so the
/// tree structure is visible but users can't try to open a README.
///
/// `rename` holds the current rename state; when the rendered node's
/// path matches `rename.target`, the row becomes an inline TextEdit.
pub fn render_twin_node(
    ui: &mut egui::Ui,
    node: &TwinNode,
    rename: &mut RenameState,
) -> TwinNodeAction {
    let mut action = TwinNodeAction::default();

    // Rename mode — replace the normal row with an inline TextEdit.
    // Shared for files and folders; the commit handler checks what
    // was at the path and renames.
    if rename.target.as_deref() == Some(node.path.as_path()) {
        let response = ui.add(
            egui::TextEdit::singleline(&mut rename.buffer)
                .desired_width(f32::INFINITY),
        );
        if rename.needs_focus {
            response.request_focus();
            rename.needs_focus = false;
        }
        // Enter → commit, Escape → cancel, loss of focus → cancel.
        if response.lost_focus() {
            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let new_name = rename.buffer.trim().to_string();
                if !new_name.is_empty() && new_name != node.name {
                    let parent = node.path.parent().unwrap_or(std::path::Path::new(""));
                    let new_path = parent.join(&new_name);
                    action.commit_rename = Some((node.path.clone(), new_path));
                } else {
                    action.cancel_rename = true;
                }
            } else {
                // Escape or click-away — treat as cancel.
                action.cancel_rename = true;
            }
        }
        return action;
    }

    if node.is_dir() {
        let id = egui::Id::new(("twin_node", node.path.as_os_str()));
        let header = egui::CollapsingHeader::new(
            egui::RichText::new(format!("📁 {}", node.name))
                .size(11.0)
                .color(egui::Color32::from_rgb(180, 200, 230)),
        )
        .id_salt(id)
        .default_open(false);
        let header_response = header
            .show(ui, |ui| {
                for child in &node.children {
                    action.merge(render_twin_node(ui, child, rename));
                }
            })
            .header_response
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        node_context_menu(&header_response, node, &mut action);
    } else {
        let (icon, color) = if node.is_modelica {
            ("📄", egui::Color32::from_rgb(220, 220, 160))
        } else {
            ("·", egui::Color32::from_rgb(110, 110, 120))
        };
        let row = egui::Button::selectable(
            false,
            egui::RichText::new(format!("{icon}  {}", node.name))
                .size(11.0)
                .color(color),
        );
        let resp = ui.add_enabled(node.is_modelica || !node.is_modelica, row);
        if resp.clicked() && node.is_modelica {
            action.open = Some(node.path.clone());
        }
        node_context_menu(&resp, node, &mut action);
    }
    action
}

/// Attach a right-click context menu to `resp` with Rename.
/// Kept small today — Delete + New File land in the next phase
/// alongside filesystem guards for dangerous operations.
fn node_context_menu(
    resp: &egui::Response,
    node: &TwinNode,
    action: &mut TwinNodeAction,
) {
    resp.context_menu(|ui| {
        if ui.button("✏  Rename").clicked() {
            action.rename = Some(node.path.clone());
            ui.close();
        }
    });
}

/// Render every in-memory model the user has created this session.
/// Returns the id of the one the user clicked (if any).
///
/// `active_id` is the currently-open model's `model_path`, used to mark
/// the active entry so the user can see which one is being edited.
fn render_in_memory_models(
    ui: &mut egui::Ui,
    entries: &[InMemoryEntry],
    active_id: Option<&str>,
) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut clicked = None;
    for entry in entries {
        let is_active = active_id == Some(entry.id.as_str());
        let label = if is_active {
            egui::RichText::new(format!("💾 {} ✏️", entry.display_name))
                .size(11.0)
                .color(egui::Color32::YELLOW)
                .strong()
        } else {
            egui::RichText::new(format!("💾 {}", entry.display_name))
                .size(11.0)
                .color(egui::Color32::from_rgb(220, 220, 180))
        };
        let resp = ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.add(egui::Label::new(label).selectable(false).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
        }).inner;
        if resp.clicked() && !is_active {
            clicked = Some(entry.id.clone());
        }
    }
    clicked
}

/// Resolve a Package Browser tree row id to its fully-qualified MSL
/// path. The browser stores MSL entries with the `msl_path:` prefix
/// (which canonicalises by stripping the leading `Modelica.` and
/// re-adding it on lookup); other libraries use the bare id. This
/// helper centralises the conversion so click handlers don't
/// open-code the prefix.
fn msl_path_for_id(id: &str, library: &ModelLibrary) -> String {
    if let Some(rest) = id.strip_prefix("msl_path:") {
        format!("Modelica.{}", rest)
    } else if matches!(library, ModelLibrary::MSL) {
        format!("Modelica.{}", id)
    } else {
        id.to_string()
    }
}

/// Peek the Modelica class kind by reading the first few lines of a
/// `.mo` file. Cheap (~one disk page on cold start, free on warm),
/// runs once per file at scan time and never re-runs during render.
///
/// Strips leading `// line comments`, `/* block comments */`, and
/// `within …;` headers, then takes the first significant token after
/// the standard qualifier prefixes (`encapsulated` / `final` /
/// `partial` / `expandable` / `operator` / `pure` / `impure` /
/// `redeclare`). Returns the raw keyword (`"model"`, `"block"`,
/// `"connector"`, ...) so the existing `kind_letter_for` mapping
/// applies uniformly.
///
/// Returns `None` only on I/O failure or files with no recognisable
/// class header in the first 50 lines (rare — one would have to
/// invent a new keyword Modelica doesn't define).
/// Parse a Modelica `.mo` file and turn its top-level class into a
/// [`PackageNode`]. When the file's class is a `package` containing
/// inner classes, the result is a [`PackageNode::Category`] whose
/// children are the inline classes (recursively, so nested packages
/// inside the same file expand too — matches OMEdit's tree shape).
/// Leaf classes (model/block/connector/...) become [`PackageNode::Model`].
///
/// Falls back to a kind-only `Model` node when the parse fails — the
/// scanner stays robust against partial / experimental files.
fn node_from_modelica_file(
    path: &std::path::Path,
    qualified: &str,
    display_name: &str,
) -> PackageNode {
    // Peek the class kind first — single-file packages need a full
    // AST walk to expose their inline children, but leaf classes
    // (model/block/connector/function/record/type) just become a
    // Model node. Skipping the full rumoca parse on every leaf
    // file in MSL is a huge speedup (most `.mo` files are leaves).
    let (kind_str, _) = peek_class_header(path);
    if kind_str.as_deref() != Some("package") {
        return leaf_model_node(qualified, display_name, kind_str);
    }
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return leaf_model_node(qualified, display_name, kind_str),
    };
    let ast = rumoca_phase_parse::parse_to_recovered_ast(&source, &path.display().to_string());
    let Some((_, top_class)) = ast.classes.iter().next() else {
        return leaf_model_node(qualified, display_name, kind_str);
    };
    class_def_to_node(path, qualified, display_name, top_class)
}

/// Recursive AST → tree node. Inner packages expand into Categories
/// with children; leaf classes become Model rows. Files are tracked
/// on every node so click-to-open can resolve the source file by
/// walking up the qualified path until an existing `.mo` is found
/// (handles directory- and single-file-package layouts uniformly).
fn class_def_to_node(
    file_path: &std::path::Path,
    qualified: &str,
    short_name: &str,
    class_def: &rumoca_session::parsing::ast::ClassDef,
) -> PackageNode {
    use rumoca_session::parsing::ClassType;
    let kind_str = class_kind_to_str(&class_def.class_type);
    let is_package = matches!(class_def.class_type, ClassType::Package);
    if is_package && !class_def.classes.is_empty() {
        let mut children: Vec<PackageNode> = class_def
            .classes
            .iter()
            .map(|(child_short, child_def)| {
                let child_qualified = format!("{}.{}", qualified, child_short);
                class_def_to_node(file_path, &child_qualified, child_short, child_def)
            })
            .collect();
        // Same OMEdit ordering inside inline-package contents:
        // UsersGuide pinned, Examples second, then folders, then
        // leaves — alphabetical within each group.
        children.sort_by_key(omedit_sort_key);
        let id = format!(
            "msl_path:{}",
            qualified.strip_prefix("Modelica.").unwrap_or(qualified)
        );
        PackageNode::Category {
            id,
            name: short_name.to_string(),
            package_path: qualified.to_string(),
            fs_path: file_path.to_path_buf(),
            children: Some(children),
            is_loading: false,
        }
    } else {
        leaf_model_node(qualified, short_name, Some(kind_str.to_string()))
    }
}

fn leaf_model_node(qualified: &str, short_name: &str, class_kind: Option<String>) -> PackageNode {
    let id = format!(
        "msl_path:{}",
        qualified.strip_prefix("Modelica.").unwrap_or(qualified)
    );
    PackageNode::Model {
        id,
        name: short_name.to_string(),
        library: ModelLibrary::MSL,
        class_kind,
    }
}

fn class_kind_to_str(kind: &rumoca_session::parsing::ClassType) -> &'static str {
    use rumoca_session::parsing::ClassType;
    match kind {
        ClassType::Model => "model",
        ClassType::Block => "block",
        ClassType::Connector => "connector",
        ClassType::Function => "function",
        ClassType::Record => "record",
        ClassType::Type => "type",
        ClassType::Package => "package",
        ClassType::Class => "class",
        ClassType::Operator => "operator",
    }
}

/// Peek the class header from a `.mo` file. Returns `(class_kind,
/// description)` — kind is the Modelica keyword, description is the
/// first quoted string after the class name (Modelica's "comment" —
/// what OMEdit/Dymola show in tooltips).
///
/// Reads up to 80 lines, strips line + block comments and
/// `within …;`, then walks tokens to find the kind keyword and the
/// following quoted docstring. The docstring may sit on the same
/// line (`model X "…"`) or the next line (the common MSL pattern
/// for packages: `package Foo` newline `  "…"`). Both are handled.
fn peek_class_header(path: &std::path::Path) -> (Option<String>, Option<String>) {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else {
        return (None, None);
    };
    let reader = std::io::BufReader::new(f);
    let mut kind: Option<String> = None;
    let mut tail_after_kind = String::new();
    let mut in_block_comment = false;
    for line in reader.lines().take(80).flatten() {
        let mut s = line.as_str().to_string();
        // Strip block comments inline. `s` is rebuilt as we walk.
        let mut cleaned = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if in_block_comment {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    in_block_comment = false;
                }
                continue;
            }
            if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                in_block_comment = true;
                continue;
            }
            if c == '/' && chars.peek() == Some(&'/') {
                // Line-comment — discard the rest.
                break;
            }
            cleaned.push(c);
        }
        s = cleaned;
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("within") {
            continue;
        }
        if kind.is_none() {
            // Walk tokens for the first kind keyword after qualifiers.
            // Stop when we find a kind; remember the tail of that line
            // (after the kind+name) so the description can sit on the
            // same line.
            let mut tokens = trimmed.split_whitespace();
            while let Some(tok) = tokens.next() {
                match tok {
                    "encapsulated" | "final" | "partial" | "expandable" | "operator" | "pure"
                    | "impure" | "redeclare" | "replaceable" => continue,
                    "model" | "block" | "connector" | "function" | "record" | "type"
                    | "package" | "class" => {
                        kind = Some(tok.to_string());
                        // Consume the name token and grab everything after.
                        let _name = tokens.next();
                        tail_after_kind = tokens.collect::<Vec<_>>().join(" ");
                        break;
                    }
                    _ => return (None, None),
                }
            }
            // Try same-line docstring first.
            if let Some(d) = first_quoted(&tail_after_kind) {
                return (kind, Some(d));
            }
            // Otherwise keep scanning for the docstring on later lines.
            continue;
        }
        // Past the class header — look for the first quoted string,
        // bail at the first `extends`/`import`/equation/declaration
        // marker so we don't pick up an internal `"..."` string.
        if trimmed.starts_with("extends")
            || trimmed.starts_with("import")
            || trimmed.starts_with("annotation")
            || trimmed.starts_with("equation")
            || trimmed.starts_with("algorithm")
            || trimmed.starts_with("end ")
        {
            return (kind, None);
        }
        if let Some(d) = first_quoted(trimmed) {
            return (kind, Some(d));
        }
    }
    (kind, None)
}

/// Extract the first double-quoted string from a Modelica fragment.
/// Honors `\"` escapes. Returns `None` if no closed quoted string is
/// present.
fn first_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = s.find('"')?;
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            out.push(bytes[i + 1] as char);
            i += 2;
            continue;
        }
        if c == b'"' {
            return Some(out);
        }
        out.push(c as char);
        i += 1;
    }
    None
}


/// Add an instance of an MSL class as a sub-component of the
/// currently-active canvas tab. Mirrors what the Component Palette
/// click does, so the read-only / class-resolution / placement logic
/// stays in one place (the [`crate::api_edits::AddModelicaComponent`]
/// observer + `apply_ops`).
fn instantiate_on_active_canvas(
    world: &mut World,
    msl_path: &str,
    display_name: &str,
) {
    // Resolve target doc.
    let active_doc = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document);
    let Some(doc_id) = active_doc else {
        bevy::log::info!(
            "[PackageBrowser] double-click on `{}` ignored — no active document",
            msl_path
        );
        return;
    };
    // Class to add into = drilled-in pin if any, else first non-package.
    let drilled_in = world
        .get_resource::<crate::ui::panels::canvas_diagram::DrilledInClassNames>()
        .and_then(|m| m.get(doc_id).map(str::to_string));
    let class = drilled_in
        .or_else(|| {
            // Read first non-package class from the per-doc Index;
            // sees optimistic patches and avoids walking the AST.
            let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
            let host = registry.host(doc_id)?;
            host.document()
                .index()
                .classes
                .values()
                .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                .map(|c| c.name.clone())
        })
        .unwrap_or_default();
    if class.is_empty() {
        bevy::log::info!(
            "[PackageBrowser] double-click on `{}` ignored — no target class",
            msl_path
        );
        return;
    }
    // Synthesise an instance name from the short tail. Lower-case
    // first char + a small numeric suffix to avoid collisions on
    // repeat double-clicks. The user can rename via the inspector.
    let short = msl_path.rsplit('.').next().unwrap_or(msl_path);
    let mut base = String::new();
    for (i, ch) in short.chars().enumerate() {
        if i == 0 {
            base.push(ch.to_ascii_lowercase());
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            base.push(ch);
        }
    }
    if base.is_empty() {
        base.push_str("inst");
    }
    let name = format!(
        "{base}{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u32 % 10_000)
            .unwrap_or(0)
    );
    bevy::log::info!(
        "[PackageBrowser] double-click → AddModelicaComponent(`{display_name}` as `{name}` in `{class}`)"
    );

    #[cfg(feature = "lunco-api")]
    world
        .commands()
        .trigger(crate::api_edits::AddModelicaComponent {
            doc: doc_id,
            class,
            type_name: msl_path.to_string(),
            name,
            x: 0.0,
            y: 0.0,
            width: 20.0,
            height: 20.0,
            animation_ms: 0,
        });
    #[cfg(not(feature = "lunco-api"))]
    {
        let _ = (display_name, name, class);
        bevy::log::warn!(
            "[PackageBrowser] double-click instantiate requires the `lunco-api` feature"
        );
    }
}

/// Resolve an `msl_path:` id (e.g. `Blocks.ImpureRandom`) to the
/// actual MSL bundle key (e.g. `Modelica/Blocks/package.mo`). Walks
/// the dotted path in the same order the bg task used to: try
/// `<seg…>.mo`, then `<seg…>/package.mo`; first match wins.
///
/// Native: walks the `<cache>/msl/Modelica/` filesystem.
/// Wasm: walks the in-memory bundle's HashMap keys (microseconds).
fn resolve_msl_path_to_file(rel_path: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    let parts: Vec<&str> = rel_path.split('.').collect();

    #[cfg(target_arch = "wasm32")]
    {
        let bundle = match lunco_assets::msl::global_msl_source()? {
            lunco_assets::msl::MslAssetSource::InMemory(b) => b.clone(),
            _ => return None,
        };
        for end in (1..=parts.len()).rev() {
            let mut as_file = String::from("Modelica");
            for seg in &parts[..end] {
                as_file.push('/');
                as_file.push_str(seg);
            }
            let key_file = PathBuf::from(format!("{as_file}.mo"));
            if bundle.files.contains_key(&key_file) {
                return Some(key_file);
            }
            let key_pkg = PathBuf::from(format!("{as_file}/package.mo"));
            if bundle.files.contains_key(&key_pkg) {
                return Some(key_pkg);
            }
        }
        let root = PathBuf::from("Modelica/package.mo");
        if bundle.files.contains_key(&root) {
            return Some(root);
        }
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let msl_root = lunco_assets::msl_dir();
        for end in (1..=parts.len()).rev() {
            let mut as_file = msl_root.join("Modelica");
            as_file.push(parts[..end].join("/"));
            as_file.set_extension("mo");
            if as_file.exists() {
                return Some(as_file);
            }
            let mut as_pkg = msl_root.join("Modelica");
            as_pkg.push(parts[..end].join("/"));
            as_pkg.push("package.mo");
            if as_pkg.exists() {
                return Some(as_pkg);
            }
        }
        let root = msl_root.join("Modelica").join("package.mo");
        if root.is_file() {
            return Some(root);
        }
        None
    }
}

pub(crate) fn open_model(world: &mut World, id: String, name: String, library: ModelLibrary) {
    // Before navigating away, flush any in-progress work on the current
    // model into its Document. Matches the text editor's focus-loss
    // commit so the user's changes survive a round-trip.
    commit_current_model_edits(world);

    if let Some(mut state) = world.get_resource_mut::<WorkbenchState>() {
        state.is_loading = true;
    }

    // Bundled inner-class click: ids of the form
    // `bundled://<file>#<qualified>` carry a drill-in target in the
    // fragment. Strip the fragment off so the source loader still
    // resolves the file by its base id, and queue the qualified
    // path into `PendingDrillIns` keyed by that same base — the
    // file-load task will pick it up and seed `DrilledInClassNames`
    // so the canvas lands on the inner class. Same machinery MSL
    // inner-class clicks use; just routed through the bundled
    // open-handler instead of the MSL path resolver.
    let id = if let Some(hash) = id.find('#') {
        if id.starts_with("bundled://") {
            let qualified = id[hash + 1..].to_string();
            let base = id[..hash].to_string();
            if let Some(mut pending) = world
                .get_resource_mut::<crate::ui::browser_dispatch::PendingDrillIns>()
            {
                pending.queue(base.clone(), qualified);
            }
            base
        } else {
            id
        }
    } else {
        id
    };

    // Determine the loading strategy based on the ID scheme
    if id.starts_with("mem://") {
        let mem_name_str = id.strip_prefix("mem://").unwrap_or("NewModel").to_string();

        // Find the existing Document for this in-memory model. If one
        // exists (user created it earlier this session), we restore its
        // *current* source and hold on to the id so further edits keep
        // landing on the same Document. Only fall back to a fresh
        // template if nothing is registered — a defensive path; shouldn't
        // normally fire because New Model allocates up front.
        let mem_path = std::path::PathBuf::from(&id);
        let (source, doc_id) = {
            let registry = world.resource::<ModelicaDocumentRegistry>();
            match registry.find_by_path(&mem_path) {
                Some(doc) => {
                    let src = registry
                        .host(doc)
                        .map(|h| h.document().source().to_string())
                        .unwrap_or_default();
                    (src, Some(doc))
                }
                None => (
                    format!("model {}\n\nend {};\n", mem_name_str, mem_name_str),
                    None,
                ),
            }
        };

        // Compute line starts for the restored source so the code editor
        // can lay it out correctly.
        let mut line_starts = vec![0usize];
        for (i, byte) in source.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push(i + 1);
            }
        }

        if let Some(mut state) = world.get_resource_mut::<WorkbenchState>() {
            let source_arc: std::sync::Arc<str> = source.into();
            state.open_model = Some(OpenModel {
                model_path: id.clone(),
                display_name: name,
                source: source_arc.clone(),
                line_starts: line_starts.into(),
                detected_name: Some(mem_name_str),
                cached_galley: None,
                read_only: false,
                library,
            });
            state.editor_buffer = source_arc.to_string();
            state.diagram_dirty = true;
            state.is_loading = false;
        }
        // Sync into the Workspace session. Only when we actually have
        // a doc id — a missing id here means we didn't allocate (first
        // open of a "mem://" model that was never created), and the
        // Workspace can't track what it doesn't have.
        if let Some(doc_id) = doc_id {
            if let Some(mut ws) =
                world.get_resource_mut::<lunco_workbench::WorkspaceResource>()
            {
                ws.active_document = Some(doc_id);
            }
        }

        // Open (or focus) the tab for this in-memory model.
        // Panels render inside `render_workbench`, which extracts
        // `WorkbenchLayout` from the world for the duration — touching
        // the resource directly here would panic. Fire an event; the
        // workbench's `on_open_tab` observer picks it up after the
        // render system completes.
        if let Some(doc) = doc_id {
            world
                .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
                .ensure(doc);
            world.commands().trigger(lunco_workbench::OpenTab {
                kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                instance: doc.raw(),
            });
        }
        let _ = id;
        return;
    }

    // Background load for all other types (Disk or Bundled).
    //
    // Dedup: if a tab is already open for this id (registered
    // doc whose origin path matches `id`) — focus it. If a load
    // for this id is already in flight (user double-clicked while
    // the bg parse is running), focus the placeholder tab we
    // opened on the first click. Without these the second click
    // reserves a new DocumentId, spawns a second parse, and the
    // user ends up with a duplicate tab once both land.
    // For `msl_path:Foo.Bar` ids, pre-resolve to the actual MSL file
    // path on the main thread (HashMap lookups, microseconds) so the
    // doc's `DocumentOrigin::File.path` carries a real bundle key
    // (`Modelica/Blocks/Sources.mo`). The wasm short-circuit in
    // `drive_engine_sync` looks the path up in the pre-parsed MSL
    // bundle to skip the worker round-trip; without a real key it
    // always misses, and the worker spends minutes re-parsing a
    // 152 KB file the bundle already has the AST for.
    let path_buf: std::path::PathBuf = if let Some(rel) = id.strip_prefix("msl_path:") {
        resolve_msl_path_to_file(rel).unwrap_or_else(|| std::path::PathBuf::from(&id))
    } else {
        std::path::PathBuf::from(&id)
    };
    let already_open: Option<lunco_doc::DocumentId> = world
        .resource::<ModelicaDocumentRegistry>()
        .find_by_path(&path_buf);
    if let Some(doc) = already_open {
        world
            .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
            .ensure(doc);
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
            instance: doc.raw(),
        });
        return;
    }
    if let Some(&doc) = world
        .resource::<PackageTreeCache>()
        .loading_ids
        .get(&id)
    {
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
            instance: doc.raw(),
        });
        return;
    }
    // Reserve a `DocumentId` up front so the bg task can build a
    // fully-parsed `ModelicaDocument` off the UI thread (rumoca
    // parses can take 100s of ms on MSL package files); the main
    // thread only does the cheap `install_prebuilt` HashMap insert.
    let reserved_doc_id = world
        .resource_mut::<ModelicaDocumentRegistry>()
        .reserve_id();
    world
        .resource_mut::<PackageTreeCache>()
        .loading_ids
        .insert(id.clone(), reserved_doc_id);
    // Open the tab immediately so the user gets visible feedback
    // even though the parse is still running off-thread. The model
    // view paints a "Loading…" overlay while the registry has no
    // host for the reserved id yet.
    world
        .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
        .ensure(reserved_doc_id);
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: reserved_doc_id.raw(),
    });
    let writable = matches!(library, ModelLibrary::User);
    let origin = lunco_doc::DocumentOrigin::File {
        path: path_buf,
        writable,
    };
    let pool = AsyncComputeTaskPool::get();
    let id_clone = id.clone();
    let name_clone = name.clone();
    let name_result = name.clone();
    let lib_clone = library.clone();

    let task = pool.spawn(async move {
        let source_text = if id_clone.starts_with("bundled://") {
            let filename = id_clone.strip_prefix("bundled://").unwrap_or("");
            crate::models::get_model(filename).unwrap_or("").to_string()
        } else if let Some(rel_path) = id_clone.strip_prefix("msl_path:") {
            // Walk up the dotted path looking for the file that owns
            // this class. MSL uses three storage conventions, often
            // mixed within one library:
            //
            //   1. Directory-structured: `Mechanics/Rotational/Examples/Backlash.mo`
            //      — one class per file, file path mirrors qualified name.
            //   2. Single-file package: `Blocks/Continuous.mo` containing
            //      `package Continuous ... block Integrator ... end;`
            //      — qualified `Blocks.Continuous.Integrator` lives in
            //      `Continuous.mo`.
            //   3. Inline-in-`package.mo`: `Blocks/package.mo` containing
            //      `package Blocks ... package Examples ... model PID ...`
            //      — qualified `Blocks.Examples.PID` lives in
            //      `Blocks/package.mo`.
            //
            // For each level walking up, try both the `.mo` file and
            // the `/package.mo` of the directory at that level. First
            // existing match wins. Drill-in (queued separately) lands
            // on the specific class within the loaded file.
            //
            // Native: filesystem under `<cache>/msl/Modelica/`.
            // Wasm: in-memory bundle (`MslAssetSource::InMemory`) keyed
            //       by the same relative paths the filesystem uses, so
            //       the same walking algorithm applies — we just call
            //       `.contains_key` instead of `.exists()` and route
            //       reads through the bundle rather than `std::fs`.
            let parts: Vec<&str> = rel_path.split('.').collect();
            #[cfg(target_arch = "wasm32")]
            {
                use std::path::PathBuf;
                let bundle = match lunco_assets::msl::global_msl_source() {
                    Some(lunco_assets::msl::MslAssetSource::InMemory(b)) => Some(b.clone()),
                    _ => None,
                };
                let mut chosen_key: Option<PathBuf> = None;
                if let Some(b) = bundle.as_ref() {
                    'walk: for end in (1..=parts.len()).rev() {
                        let mut as_file = String::from("Modelica");
                        for seg in &parts[..end] {
                            as_file.push('/');
                            as_file.push_str(seg);
                        }
                        let key_file = PathBuf::from(format!("{as_file}.mo"));
                        if b.files.contains_key(&key_file) {
                            chosen_key = Some(key_file);
                            break 'walk;
                        }
                        let key_pkg = PathBuf::from(format!("{as_file}/package.mo"));
                        if b.files.contains_key(&key_pkg) {
                            chosen_key = Some(key_pkg);
                            break 'walk;
                        }
                    }
                    if chosen_key.is_none() {
                        let root_pkg = PathBuf::from("Modelica/package.mo");
                        if b.files.contains_key(&root_pkg) {
                            chosen_key = Some(root_pkg);
                        }
                    }
                }
                let bytes = chosen_key
                    .as_ref()
                    .and_then(|k| bundle.as_ref().and_then(|b| b.files.get(k).cloned()));
                match bytes.and_then(|v| String::from_utf8(v).ok()) {
                    Some(s) => s,
                    None => format!(
                        "// Could not locate `{rel_path}` in the in-memory MSL bundle.\n\
                         // (Bundle status: {})\n",
                        if bundle.is_some() {
                            "loaded but key missing"
                        } else {
                            "not yet installed (MSL fetch may still be in flight)"
                        }
                    ),
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let msl_root = lunco_assets::msl_dir();
                let mut full_path: std::path::PathBuf = msl_root.join("Modelica");
                'walk: for end in (1..=parts.len()).rev() {
                    let mut as_file = msl_root.join("Modelica");
                    as_file.push(parts[..end].join("/"));
                    as_file.set_extension("mo");
                    if as_file.exists() {
                        full_path = as_file;
                        break 'walk;
                    }
                    let mut as_pkg = msl_root.join("Modelica");
                    as_pkg.push(parts[..end].join("/"));
                    as_pkg.push("package.mo");
                    if as_pkg.exists() {
                        full_path = as_pkg;
                        break 'walk;
                    }
                }
                if !full_path.is_file() {
                    full_path = msl_root.join("Modelica").join("package.mo");
                }
                std::fs::read_to_string(&full_path).unwrap_or_else(|e| {
                    format!("// Error reading {}\n// {:?}", full_path.display(), e)
                })
            }
        } else {
            // Default User model load. Wasm has no filesystem at runtime
            // — return a placeholder rather than panicking inside
            // `read_to_string` (the libstd wasm32-unknown-unknown stub
            // resolves relative paths via `current_dir()` which fatals
            // with "no filesystem on this platform").
            #[cfg(target_arch = "wasm32")]
            {
                format!(
                    "// User-model load not supported on web (no filesystem).\n\
                     // id = {id_clone}\n"
                )
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                let path = std::path::PathBuf::from(&id_clone);
                std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    format!("// Error reading {:?}\n// {:?}", path, e)
                })
            }
        };

        // Compute line starts (zero allocation scan)
        let mut line_starts = vec![0];
        for (i, byte) in source_text.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                line_starts.push(i + 1);
            }
        }

        // Use the name from the UI immediately instead of parsing the whole AST.
        let detected_name = Some(name_clone);

        // Pre-compute text layout in the background. Skip on wasm —
        // `AsyncComputeTaskPool` runs cooperatively on the main thread
        // there, so this 184 KB-MSL-tokeniser walk is exactly the
        // stall the user sees clicking a Modelica.Blocks node. egui
        // recomputes layout on first render anyway, but only for the
        // visible rect (not the whole 150 KB file), which is cheap.
        // Native: keep the pre-compute — real worker threads, free.
        #[cfg(target_arch = "wasm32")]
        let layout_job: Option<bevy_egui::egui::text::LayoutJob> = None;
        #[cfg(not(target_arch = "wasm32"))]
        let layout_job = {
            let style = egui::Style::default();
            let mut job =
                crate::ui::panels::code_editor::modelica_layouter(&style, &source_text);
            job.wrap.max_width = f32::INFINITY;
            Some(job)
        };

        // Heavy: rumoca parse happens here, off the UI thread. The
        // resulting `ModelicaDocument` carries its own copy of the
        // source; we still return `source: Arc<str>` for the
        // editor / open_model fields so the main thread doesn't
        // need to clone the doc's source out under a lock.
        let doc = crate::document::ModelicaDocument::with_origin(
            reserved_doc_id,
            source_text.clone(),
            origin,
        );

        FileLoadResult {
            id: id_clone,
            name: name_result,
            library: lib_clone,
            source: source_text.into(),
            line_starts: line_starts.into(),
            detected_name,
            layout_job,
            doc_id: reserved_doc_id,
            doc,
        }
    });

    if let Some(mut cache) = world.get_resource_mut::<PackageTreeCache>() {
        cache.file_tasks.push(task);
    }
}


// The legacy "New Model" modal (name-prompt dialog) used to live here.
// VS Code's one-click "New Untitled" flow replaces it — the ➕
// buttons fire `CreateNewScratchModel`, the observer in
// `ui::commands` picks the next free `UntitledN` name, allocates the
// doc, and opens a tab. Rename is deferred to Save-As.

/// Render one named root from [`PackageTreeCache::roots`] inline at
/// the caller's egui cursor. Used by the Twin panel's per-domain
/// `BrowserSection`s (today: `ModelicaSection`'s MSL and Bundled
/// sub-groups) to surface the package tree without duplicating the
/// `render_node` recursion or re-implementing lazy-load + dispatch.
///
/// The root's own header is skipped — callers wrap this in their own
/// `CollapsingHeader`. Children render in the existing PackageBrowser
/// styling. Lazy-load tasks land in the cache and are drained by
/// [`handle_package_loading_tasks`] as before.
///
/// `root_id` is the stable id assigned by [`PackageTreeCache::new`] —
/// `"msl_root"` for the Modelica Standard Library and
/// `"bundled_root"` for bundled examples. Unknown ids are silently
/// no-op (caller's collapsing header just shows blank).
pub(crate) fn render_root_subtree(world: &mut World, ui: &mut egui::Ui, root_id: &str) {
    let active_path = world
        .get_resource::<WorkbenchState>()
        .and_then(|s| s.open_model.as_ref().map(|m| m.model_path.clone()));
    let active_path_ref = active_path.as_deref();
    // Active drill-in target for the foreground tab — `RocketStage`
    // when the user has clicked into the inner class of a bundled
    // package, `AnnotatedRocketStage` when the package itself is
    // selected. Used for tree-row highlighting so the inner-class
    // bundled rows light up alongside their containing file.
    let active_drill: Option<String> = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
        .and_then(|doc| {
            world
                .get_resource::<crate::ui::panels::canvas_diagram::DrilledInClassNames>()
                .and_then(|m| m.get(doc).map(str::to_string))
        });
    let active_drill_ref = active_drill.as_deref();
    let theme = world
        .get_resource::<lunco_theme::Theme>()
        .cloned()
        .unwrap_or_else(lunco_theme::Theme::dark);

    let mut action: Option<PackageAction> = None;
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
            children,
            fs_path,
            package_path,
            is_loading,
            id,
            ..
        } = root
        {
            if let Some(kids) = children {
                // Clamp width and truncate long labels — same defence
                // against panel-overrun the standalone PackageBrowser
                // applies. Without it, deep MSL paths would spill past
                // the side-panel edge and clip behind the right dock.
                ui.set_max_width(ui.available_width());
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for child in kids.iter_mut() {
                    if let Some(a) = render_node(
                        child,
                        ui,
                        active_path_ref,
                        active_drill_ref,
                        0,
                        &mut cache.tasks,
                        &theme,
                    ) {
                        action = Some(a);
                    }
                }
            } else if !*is_loading {
                // First-time lazy load — same path as the standalone
                // PackageBrowser. Subsequent renders reuse the cached
                // children once the task completes.
                *is_loading = true;
                let pool = AsyncComputeTaskPool::get();
                let parent_id = id.clone();
                let scan_dir = fs_path.clone();
                let pkg_path = package_path.clone();
                let task = pool.spawn(async move {
                    let children = scan_msl_dir(&scan_dir, pkg_path);
                    ScanResult { parent_id, children }
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
    if let Some(a) = action {
        match a {
            PackageAction::Open(id, name, lib) => {
                queue_drill_in_if_inline(world, &id, &lib);
                open_model(world, id, name, lib);
            }
            PackageAction::Instantiate {
                msl_path,
                display_name,
            } => instantiate_on_active_canvas(world, &msl_path, &display_name),
            PackageAction::DragStart { msl_path } => stash_drag_payload(world, &msl_path),
        }
    }
}

/// Queue a drill-in target before opening, so the canvas projector
/// lands on the specific class the user clicked instead of the
/// containing file's first non-package class. Critical for inline
/// classes inside single-file packages (e.g. clicking
/// `Modelica.Blocks.Continuous.Derivative` opens `Continuous.mo`
/// — without drill-in the diagram tries to render every class in
/// the 100KB+ file).
fn queue_drill_in_if_inline(world: &mut World, id: &str, library: &ModelLibrary) {
    if !matches!(library, ModelLibrary::MSL) {
        return;
    }
    let qualified = msl_path_for_id(id, library);
    world
        .resource_mut::<crate::ui::browser_dispatch::PendingDrillIns>()
        .queue(id.to_string(), qualified);
}

/// Look up the MSL component def by path and stash it as the active
/// drag payload. Mirrors the Component Palette's drag-start path —
/// shared `ComponentDragPayload` resource means the canvas drop
/// handler treats palette and tree drags identically. No-op when the
/// path isn't in the static MSL library (e.g. Bundled-only entries
/// or user files); the user can still double-click to add at origin.
fn stash_drag_payload(world: &mut World, msl_path: &str) {
    if let Some(def) = crate::visual_diagram::msl_component_by_path(msl_path) {
        world
            .get_resource_or_insert_with::<crate::ui::panels::palette::ComponentDragPayload>(
                Default::default,
            )
            .def = Some(def);
    }
}
