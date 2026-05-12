//! Backend scanning logic for the Package Browser.

use bevy::prelude::*;
use std::path::{Path, PathBuf};
use crate::ui::state::ModelLibrary;
use super::types::{PackageNode, TwinNode};
use super::cache::{ScanResult, FileLoadResult, TwinState};

// ─── Twin / Workspace Scanning ───────────────────────────────────────────────

pub fn scan_twin_folder(root: PathBuf) -> TwinState {
    let root_node = TwinNode {
        path: root.clone(),
        name: root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        children: Vec::new(), // FIXME: scan_children needs to return Vec<TwinNode>
        is_modelica: false,
    };
    TwinState {
        root,
        root_node,
    }
}

fn scan_children(dir: &Path) -> Vec<PackageNode> {
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
        
        if is_dir {
            let children = scan_children(&path);
            out.push(PackageNode::Category {
                id: format!("twin_{}", path.display()),
                name,
                package_path: String::new(), // Not used for Twin today
                fs_path: path,
                children: Some(children),
                is_loading: false,
            });
        } else if is_modelica {
            let display_name = name.strip_suffix(".mo").unwrap_or(&name).to_string();
            out.push(PackageNode::Model {
                id: format!("file://{}", path.display()),
                name: display_name,
                library: ModelLibrary::User,
                class_kind: None, // Will be filled on demand or lazy
            });
        }
    }
    out.sort_by(|a, b| {
        let a_is_dir = matches!(a, PackageNode::Category { .. });
        let b_is_dir = matches!(b, PackageNode::Category { .. });
        b_is_dir.cmp(&a_is_dir).then_with(|| a.name().cmp(b.name()))
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

// ─── MSL Scanning ────────────────────────────────────────────────────────────

pub fn scan_msl_dir(dir: &Path, package_path: String) -> Vec<PackageNode> {
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

#[cfg(not(target_arch = "wasm32"))]
fn scan_msl_dir_native(dir: &Path, package_path: String) -> Vec<PackageNode> {
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
                    continue;
                }
                let display_name = name.strip_suffix(".mo").unwrap_or(&name).to_string();
                let qualified = format!("{}.{}", package_path, display_name);
                results.push(node_from_modelica_file(&path, &qualified, &display_name));
            }
        }
    }

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

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum SortGroup {
    UsersGuide,
    Examples,
    SubPackage,
    Leaf(LeafKind),
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum LeafKind {
    Model, Block, Connector, Record, Function, Type, Constant, Other,
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

fn node_from_modelica_file(path: &Path, qualified: &str, display_name: &str) -> PackageNode {
    let kind = peek_class_kind_from_source_file(path);
    PackageNode::Model {
        id: format!("msl_path:{}", qualified),
        name: display_name.to_string(),
        library: ModelLibrary::MSL,
        class_kind: kind,
    }
}

fn peek_class_kind_from_source_file(path: &Path) -> Option<String> {
    let Ok(src) = std::fs::read_to_string(path) else { return None; };
    peek_class_kind_from_source(&src)
}

pub fn peek_class_kind_from_source(src: &str) -> Option<String> {
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

fn class_def_to_node(
    path: &Path,
    qualified: &str,
    short_name: &str,
    def: &rumoca_session::parsing::ast::ClassDef,
) -> PackageNode {
    use rumoca_session::parsing::ClassType;
    let is_package = matches!(def.class_type, ClassType::Package);
    if is_package && !def.classes.is_empty() {
        let mut children: Vec<PackageNode> = def
            .classes
            .iter()
            .map(|(n, c)| class_def_to_node(path, &format!("{qualified}.{n}"), n, c))
            .collect();
        children.sort_by_key(omedit_sort_key);
        PackageNode::Category {
            id: format!("msl_path:{}", qualified),
            name: short_name.to_string(),
            package_path: qualified.to_string(),
            fs_path: path.to_path_buf(),
            children: Some(children),
            is_loading: false,
        }
    } else {
        PackageNode::Model {
            id: format!("msl_path:{}", qualified),
            name: short_name.to_string(),
            library: ModelLibrary::MSL,
            class_kind: Some(format!("{:?}", def.class_type).to_lowercase()),
        }
    }
}

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
