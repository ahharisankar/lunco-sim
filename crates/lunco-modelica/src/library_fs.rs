//! MSL filesystem layout — qualified-name → on-disk (or in-memory)
//! `.mo` file resolution.
//!
//! Pure path / index logic, no parsing. The resolver tier consumed
//! by `class_cache` (engine-backed loader) and the drill-in / open
//! flows that need a file path before reading source.
//!
//! Two indices, both lazy:
//!
//! - [`class_to_file_index`] — `qualified → PathBuf` for every
//!   class the visual palette knows about. Built from
//!   [`crate::visual_diagram::msl_class_library`].
//! - [`library_fs_index`] — last-segment → `[qualified]` plus
//!   `qualified → PathBuf` for short-form references like
//!   `Rotational.Interfaces.Flange_a` that need prefix-rewrite.
//!
//! [`locate_library_file`] is the single source-of-truth resolver: it
//! walks the in-memory bundle (web) or filesystem roots (native,
//! including extra libraries like ThermofluidStream) to map any
//! qualified name to its containing file.

use bevy::log::info;

pub fn class_to_file_index(
) -> &'static std::collections::HashMap<String, std::path::PathBuf> {
    use std::sync::OnceLock;
    static INDEX: OnceLock<std::collections::HashMap<String, std::path::PathBuf>> =
        OnceLock::new();
    static EMPTY: OnceLock<std::collections::HashMap<String, std::path::PathBuf>> =
        OnceLock::new();

    if let Some(idx) = INDEX.get() {
        return idx;
    }
    // On web, the palette library is empty until the MSL bundle has
    // been fetched + decompressed (see `msl_class_library` for the
    // same trick). If we'd `OnceLock::set` an empty map here, the
    // index would stay empty for the lifetime of the page even after
    // MSL lands. So: return an empty placeholder *without* memoising,
    // so the next caller retries the build.
    let lib = crate::visual_diagram::msl_class_library();
    if lib.is_empty() {
        return EMPTY.get_or_init(std::collections::HashMap::new);
    }
    INDEX.get_or_init(build_class_to_file_index)
}

fn build_class_to_file_index(
) -> std::collections::HashMap<String, std::path::PathBuf> {
    let start = web_time::Instant::now();
    let lib = crate::visual_diagram::msl_class_library();
    let mut map = std::collections::HashMap::with_capacity(lib.len());
    for comp in lib {
        if let Some(path) = locate_library_file(&comp.name) {
            map.insert(comp.name.clone(), path);
        }
    }
    info!(
        "[MslFs] MSL class index built: {} classes in {:?}",
        map.len(),
        start.elapsed()
    );
    map
}

pub fn locate_library_file(qualified: &str) -> Option<std::path::PathBuf> {
    let segments: Vec<&str> = qualified.split('.').collect();
    if segments.is_empty() {
        return None;
    }

    // 1. In-memory bundle — populated on web by `MslRemotePlugin`.
    //    Returns relative paths (e.g. `Modelica/Blocks/package.mo`).
    if let Some(lunco_assets::msl::MslAssetSource::InMemory(in_mem)) =
        lunco_assets::msl::global_msl_source()
    {
        for i in (1..=segments.len()).rev() {
            let prefix: std::path::PathBuf = segments[..i].iter().collect();
            // At any depth: prefer a directory holding `package.mo`
            // over a flat `.mo`, so `Modelica.Blocks` resolves to
            // `Modelica/Blocks/package.mo` (the package) rather than
            // falling up to `Modelica/package.mo` (the grandparent).
            let pkg = prefix.join("package.mo");
            if in_mem.files.contains_key(&pkg) {
                return Some(pkg);
            }
            let flat = prefix.with_extension("mo");
            if in_mem.files.contains_key(&flat) {
                return Some(flat);
            }
        }
        return None;
    }

    // 2. Filesystem path — native fallback. On wasm without a bundle
    //    installed there's nothing to look at; this whole branch is
    //    cfg'd out so we don't accidentally compile `Path::exists()`
    //    on a target that can't satisfy it.
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Search the MSL cache first, then any extra libraries
        // installed via `lunco-assets`. Extra-library cache layout
        // (per `Assets.toml`'s `dest = "<name>"`) puts the unpacked
        // archive at `<cache>/<name>/`, with the actual Modelica
        // package one level down (GitHub archive convention). The
        // pairs below mirror `msl_indexer.rs::extra_libraries` —
        // adding a library there + here is the two-line surface
        // for new third-party libs.
        let mut roots: Vec<std::path::PathBuf> = vec![lunco_assets::msl_dir()];
        let cache_root = lunco_assets::cache_dir();
        // TODO(library-auto-discovery): replace this hardcoded list
        // with `lunco_assets::extra_library_roots()` that scans
        // `cache_dir` for installed libraries. Each lands at
        // `cache_dir/<dest>/<package>/package.mo` per Assets.toml.
        // Mirror the change in `bin/msl_indexer.rs::extra_libraries`.
        for cache_subdir in ["thermofluidstream"] {
            let p = cache_root.join(cache_subdir);
            if p.exists() {
                roots.push(p);
            }
        }
        for root in &roots {
            for i in (1..=segments.len()).rev() {
                let mut dir = root.clone();
                for seg in &segments[..i] {
                    dir.push(seg);
                }
                let pkg = dir.join("package.mo");
                if pkg.exists() {
                    return Some(pkg);
                }
                let flat = dir.with_extension("mo");
                if flat.exists() {
                    return Some(flat);
                }
            }
        }
    }
    None
}

pub fn resolve_class_path_indexed(qualified: &str) -> Option<std::path::PathBuf> {
    class_to_file_index().get(qualified).cloned()
}

// ═══════════════════════════════════════════════════════════════════
// Filesystem-derived MSL resolver
// ═══════════════════════════════════════════════════════════════════
//
// The hardcoded `("Rotational", "Modelica.Mechanics.Rotational")`
// style alias table gets stale the moment MSL reorganizes. Walk the
// filesystem once and build the head-index from what's actually there:
//
//   by_head["Rotational"] = ["Modelica.Mechanics.Rotational"]
//   by_head["Blocks"]     = ["Modelica.Blocks", "Modelica.ComplexBlocks"]
//
// When a short-form ref `Rotational.Interfaces.Flange_a` comes in
// and `locate_library_file` can't find `Rotational/` at MSL root, we
// look `Rotational` up in `by_head`, prefix-rewrite, retry.
//
// Each entry here is a *package container* — a directory with
// `package.mo` or a flat `.mo` file immediately under some parent.
// Classes nested *inside* `.mo` files (e.g. `Modelica.Units.SI`
// lives inside `Modelica/Units.mo`) don't appear as filesystem
// entries; those still need either explicit user imports or a
// loaded-file import-scope scan.

#[derive(Debug, Default)]
pub struct LibraryFsIndex {
    /// Last-segment → full qualified names. `"Rotational"` may map
    /// to multiple fully-qualified packages; resolver tries each.
    pub by_head: std::collections::HashMap<String, Vec<String>>,
    /// Full qualified name → on-disk file.
    pub qualified_to_path: std::collections::HashMap<String, std::path::PathBuf>,
}

pub fn library_fs_index() -> &'static LibraryFsIndex {
    use std::sync::OnceLock;
    static INDEX: OnceLock<LibraryFsIndex> = OnceLock::new();
    INDEX.get_or_init(build_library_fs_index)
}

fn build_library_fs_index() -> LibraryFsIndex {
    let start = web_time::Instant::now();
    let Some(root) = lunco_assets::msl_source_root_path() else {
        return LibraryFsIndex::default();
    };
    let mut index = LibraryFsIndex::default();
    walk_library_fs(&root, &root, &[], &mut index);
    info!(
        "[MslFs] MSL fs index built: {} qualified paths, {} distinct heads in {:?}",
        index.qualified_to_path.len(),
        index.by_head.len(),
        start.elapsed()
    );
    index
}

fn walk_library_fs(
    root: &std::path::Path,
    dir: &std::path::Path,
    prefix: &[String],
    index: &mut LibraryFsIndex,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Record the package itself if dir has a package.mo (and it's
    // not the MSL root).
    if !prefix.is_empty() && dir.join("package.mo").exists() {
        let qualified = prefix.join(".");
        index
            .qualified_to_path
            .insert(qualified.clone(), dir.join("package.mo"));
        if let Some(head) = prefix.last() {
            index
                .by_head
                .entry(head.clone())
                .or_default()
                .push(qualified);
        }
    }
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let path = entry.path();
        let file_name = entry.file_name();
        let name_str = match file_name.to_str() {
            Some(s) => s,
            None => continue,
        };
        if file_type.is_dir() {
            // Skip MSL's Resources / test dirs.
            if matches!(name_str, "Resources" | "Images" | "test") {
                continue;
            }
            let mut next = prefix.to_vec();
            next.push(name_str.to_string());
            walk_library_fs(root, &path, &next, index);
        } else if file_type.is_file()
            && name_str.ends_with(".mo")
            && name_str != "package.mo"
        {
            let stem = name_str.trim_end_matches(".mo").to_string();
            let mut full = prefix.to_vec();
            full.push(stem.clone());
            let qualified = full.join(".");
            index.qualified_to_path.insert(qualified.clone(), path);
            index
                .by_head
                .entry(stem)
                .or_default()
                .push(qualified);
        }
    }
    let _ = root;
}

/// Try to resolve a short-form dotted ref by prefix-rewriting its
/// head against the filesystem head index. Returns the rewritten
/// full qualified name (e.g. `Rotational.Interfaces.Flange_a` →
/// `Modelica.Mechanics.Rotational.Interfaces.Flange_a`) if exactly
/// one head match exists that also resolves to an actual file when
/// combined with the remaining segments. `None` if ambiguous, no
/// head match, or already starts with a qualified path that resolves.
pub fn resolve_library_head_prefix(qualified: &str) -> Option<String> {
    // Direct hit first — head is already at MSL root.
    if locate_library_file(qualified).is_some() {
        return Some(qualified.to_string());
    }
    let (head, rest) = qualified.split_once('.').unwrap_or((qualified, ""));
    let index = library_fs_index();
    let candidates = index.by_head.get(head)?;
    // Refuse to guess when the head is ambiguous. `Logical` appears
    // in `Modelica.Blocks.Logical`, `Modelica.Clocked...Logical`,
    // etc. — picking the first match is wrong for 90% of callers.
    // Only rewrite when the filesystem gives us a unique answer.
    // For the right ambiguous case, rumoca's §5 scope walk should
    // find the actual target via imports in enclosing packages
    // (which the caller has ensured are loaded).
    if candidates.len() > 1 {
        return None;
    }
    for full_head in candidates {
        let candidate = crate::ast_extract::qualify(&full_head, rest);
        if locate_library_file(&candidate).is_some() {
            return Some(candidate);
        }
    }
    None
}
