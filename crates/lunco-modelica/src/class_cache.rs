//! Two-tier class cache: `FileCache` (PathBuf → parsed file) +
//! `ClassCache` (qualified name → class entry sharing a FileCache
//! parse).
//!
//! # Why two tiers
//!
//! MSL files come in two shapes: **own-file classes**
//! (`Capacitor.mo` → one class) and **package-aggregated files**
//! (`Continuous.mo` → holds `Der`, `Integrator`, `PID`, `FirstOrder`,
//! …). A qualified-name-only cache wastes work on the second shape:
//! drilling into `Der` parses `Continuous.mo`; drilling into
//! `Integrator` tomorrow re-parses it. A file-path cache keyed by
//! the `.mo` file turns sibling drill-ins into free lookups because
//! the one `Arc<StoredDefinition>` is shared by every class inside.
//!
//! The generic `ResourceCache` engine from [`lunco_cache`] drives
//! both tiers — only the loader differs. `ClassCache` doesn't
//! spawn tasks itself; it chases pending qualified names by
//! watching `FileCache` each frame and promoting `pending → ready`
//! when the file lands.
//!
//! # Layering
//!
//! ```text
//!     cache.request("Modelica.Blocks.Continuous.Der")
//!       │
//!       ├─ resolve qualified → "Continuous.mo" (static MSL index)
//!       │
//!       └─ file_cache.request(Continuous.mo)   [one parse per file]
//!            │
//!            └─ when FileEntry ready, ClassCache builds a
//!               CachedClass referencing its Arc<AstCache>.
//! ```
//!
//! ASTs and sources live once in [`FileEntry`]; every
//! [`CachedClass`] points at the same `Arc<str>` + `Arc<AstCache>`.
//! Memory cost of N classes sharing M files is O(M) parses, not
//! O(N) parses.

use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task};
use std::path::PathBuf;
use std::sync::Arc;

use crate::document::AstCache;
use lunco_cache::{ResourceCache, ResourceLoader};

// ═══════════════════════════════════════════════════════════════════
// File tier: one parse per .mo file, shared by all classes inside.
// ═══════════════════════════════════════════════════════════════════

/// One parsed `.mo` file. `source` and `ast` are `Arc` so every
/// class referencing this file shares them — many `CachedClass`
/// entries point at the same two `Arc`s.
///
/// The lenient `SyntaxCache` is *not* populated here — it would
/// double the loader's parse time for every MSL file load, which
/// is on the projection's critical path. `ModelicaDocument::from_parts`
/// builds the `SyntaxCache` inline from `source` (one extra parse
/// per opened class — paid once at open time, not on every cache
/// load).
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub source: Arc<str>,
    pub ast: Arc<AstCache>,
    /// Lenient parse cache — same source, salvaged AST. Parsed off-
    /// thread alongside the strict `ast` so consumers (drill-in
    /// install path) don't pay the lenient parse on the main thread
    /// at install time. Continuous.mo is large enough that the
    /// lenient parse alone froze the workbench for several seconds.
    pub syntax: Arc<crate::document::SyntaxCache>,
}

#[derive(Debug)]
pub enum FileLoadError {
    Io(std::io::Error),
}

impl std::fmt::Display for FileLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

pub struct ModelicaFileLoader;

impl ResourceLoader for ModelicaFileLoader {
    type Key = PathBuf;
    type Value = FileEntry;
    type Error = FileLoadError;

    fn load(&self, key: &PathBuf) -> Task<Result<FileEntry, FileLoadError>> {
        let path = key.clone();
        info!("[FileCache] scheduling load for `{}`", path.display());
        AsyncComputeTaskPool::get().spawn(async move {
            let t0 = web_time::Instant::now();
            // Read source once so we can hand it to both the
            // rumoca-session parse path (which carries its own
            // content-hash keyed artifact cache — in-memory + on-disk
            // bincode across app restarts) AND to our FileEntry for
            // UI-side text/projection consumers.
            let source = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        "[FileCache] bg task: read failed for `{}`: {}",
                        path.display(),
                        e
                    );
                    return Err(FileLoadError::Io(e));
                }
            };
            let read_done = t0.elapsed();

            // Delegate the parse to rumoca-session. Compared to
            // calling `rumoca_phase_parse::parse_to_ast` directly
            // (what `AstCache::from_source` does), this path:
            //   - Hashes the source and checks rumoca's in-memory
            //     LRU (256 entries) + on-disk bincode cache; a hit
            //     returns a ready `StoredDefinition` with ~zero
            //     parse cost.
            //   - Populates the same cache, so when rumoca-session
            //     runs a compile pipeline later over the same file,
            //     its query layer hits this cached entry instead of
            //     re-parsing.
            //   - Emits rumoca's per-phase instrumentation counters
            //     (parse calls, nanos, hits/misses) which we can
            //     surface via `rumoca_session::runtime_api` for
            //     diagnostics.
            //
            // `parse_files_parallel` is the public API that goes
            // through the artifact cache. We call it with a single
            // path; the rayon parallelism is overhead-free for a
            // 1-element input.
            // DIAGNOSTIC: same gate as `AstCache::from_source`, so a
            // single env var disables ALL rumoca calls system-wide.
            let parse_result = if std::env::var_os("LUNCO_NO_PARSE").is_some() {
                Err(anyhow::anyhow!("LUNCO_NO_PARSE diagnostic — parse skipped"))
            } else {
                rumoca_session::parsing::parse_files_parallel(&[path.clone()])
            };
            let ast = match parse_result {
                Ok(mut pairs) if !pairs.is_empty() => {
                    let (_, stored) = pairs.remove(0);
                    Arc::new(AstCache {
                        generation: 0,
                        result: Ok(Arc::new(stored)),
                    })
                }
                Ok(_) => Arc::new(AstCache {
                    generation: 0,
                    result: Err("rumoca returned no parse result".to_string()),
                }),
                Err(e) => Arc::new(AstCache {
                    generation: 0,
                    result: Err(e.to_string()),
                }),
            };
            let parse_done = t0.elapsed();
            // Build the lenient `SyntaxCache` WITHOUT running a second
            // parse. The strict pass above already returned the same
            // `StoredDefinition` rumoca's lenient parser would produce
            // for an error-free file — and that's the common MSL case.
            // Re-running `parse_to_syntax` here was catastrophically
            // slow in debug (192s for Continuous.mo, 184KB) because it
            // bypasses rumoca's artifact cache. On strict failure we
            // store an empty `SyntaxCache`; the doc's edit-driven
            // `ast_refresh` will re-derive both caches from a single
            // off-thread `parse_to_syntax` once the user pauses.
            let syntax = Arc::new(match ast.result.as_ref() {
                Ok(strict) => crate::document::SyntaxCache {
                    generation: 0,
                    ast: Arc::clone(strict),
                    has_errors: false,
                },
                Err(_) => crate::document::SyntaxCache {
                    generation: 0,
                    ast: Arc::new(
                        rumoca_session::parsing::ast::StoredDefinition::default(),
                    ),
                    has_errors: true,
                },
            });
            info!(
                "[FileCache] bg task done `{}`: read {:.1}ms parse {:.1}ms ({} bytes) {}",
                path.display(),
                read_done.as_secs_f64() * 1000.0,
                (parse_done - read_done).as_secs_f64() * 1000.0,
                source.len(),
                if ast.result.is_ok() { "ok" } else { "ERR" },
            );
            Ok(FileEntry {
                path,
                source: source.into(),
                ast,
                syntax,
            })
        })
    }
}

#[derive(Resource)]
pub struct FileCache(pub ResourceCache<ModelicaFileLoader>);

impl Default for FileCache {
    fn default() -> Self {
        Self(ResourceCache::new(ModelicaFileLoader))
    }
}

impl FileCache {
    pub fn peek(&self, path: &std::path::Path) -> Option<Arc<FileEntry>> {
        self.0.peek(&path.to_path_buf())
    }
    pub fn is_loading(&self, path: &std::path::Path) -> bool {
        self.0.is_loading(&path.to_path_buf())
    }
    pub fn request(&mut self, path: PathBuf) -> bool {
        self.0.request(path)
    }
    pub fn evict(&mut self, path: &std::path::Path) -> bool {
        self.0.evict(&path.to_path_buf())
    }
}

pub fn drive_file_cache(cache: Option<ResMut<FileCache>>) {
    let Some(mut cache) = cache else { return };
    for key in cache.0.drive() {
        match cache.0.state(&key) {
            Some(lunco_cache::ResourceState::Ready(entry)) => {
                info!(
                    "[FileCache] loaded `{}` ({} bytes)",
                    entry.path.display(),
                    entry.source.len()
                );
            }
            Some(lunco_cache::ResourceState::Failed(msg)) => {
                warn!("[FileCache] load failed for `{}`: {}", key.display(), msg);
            }
            None => {}
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Class tier: qualified name → class entry, composed on FileCache.
// ═══════════════════════════════════════════════════════════════════

/// One cached class. Points at the `source` + `ast` of the file it
/// lives in (shared `Arc`s). Downstream code walks `ast` to find
/// the specific sub-class by qualified name.
#[derive(Debug, Clone)]
pub struct CachedClass {
    pub qualified: String,
    pub source: Arc<str>,
    pub ast: Arc<AstCache>,
    pub syntax: Arc<crate::document::SyntaxCache>,
    pub file_path: PathBuf,
}

/// Terminal state for a class request.
enum ClassStatus {
    Ready(Arc<CachedClass>),
    /// We've resolved the file path and asked `FileCache` — waiting
    /// for it to land.
    PendingFile(PathBuf),
    Failed(Arc<str>),
}

/// Qualified-name → class cache. Unlike `FileCache`, this doesn't
/// own a `ResourceCache<Loader>` — there's no async work specific
/// to the class tier. It's pure bookkeeping: path resolution,
/// pending bindings, and promotion when `FileCache` resolves.
#[derive(Resource, Default)]
pub struct ClassCache {
    entries: std::collections::HashMap<String, ClassStatus>,
}

impl ClassCache {
    pub fn peek(&self, qualified: &str) -> Option<Arc<CachedClass>> {
        match self.entries.get(qualified) {
            Some(ClassStatus::Ready(c)) => Some(Arc::clone(c)),
            _ => None,
        }
    }

    pub fn is_loading(&self, qualified: &str) -> bool {
        matches!(self.entries.get(qualified), Some(ClassStatus::PendingFile(_)))
    }

    /// Tri-state state accessor for UI diagnostics: `Some(Ready(...))`,
    /// `Some(Pending)`, `Some(Failed(msg))`, or `None` for never-requested.
    pub fn state_display(&self, qualified: &str) -> Option<&'static str> {
        self.entries.get(qualified).map(|s| match s {
            ClassStatus::Ready(_) => "ready",
            ClassStatus::PendingFile(_) => "loading",
            ClassStatus::Failed(_) => "failed",
        })
    }

    pub fn failure_message(&self, qualified: &str) -> Option<Arc<str>> {
        match self.entries.get(qualified) {
            Some(ClassStatus::Failed(s)) => Some(Arc::clone(s)),
            _ => None,
        }
    }

    pub fn evict(&mut self, qualified: &str) -> bool {
        self.entries.remove(qualified).is_some()
    }

    /// Kick off a load (via `FileCache`) for this class if it
    /// isn't already cached or in-flight. Returns whether a NEW
    /// resolution happened (cache-miss path). Cheap on repeats —
    /// just a HashMap lookup.
    ///
    /// `file_cache` is passed in because two separate resources
    /// can't be fetched in one call from `World`; the caller (a
    /// Bevy system or a helper like [`request_class`]) holds both
    /// borrows.
    pub fn request(
        &mut self,
        qualified: impl Into<String>,
        file_cache: &mut FileCache,
    ) -> bool {
        let qualified = qualified.into();
        if self.entries.contains_key(&qualified) {
            // Already Ready / PendingFile / Failed — nothing to do.
            return false;
        }
        let Some(path) = resolve_msl_class_path(&qualified) else {
            self.entries.insert(
                qualified.clone(),
                ClassStatus::Failed(format!("no file for `{qualified}`").into()),
            );
            return true;
        };
        // If the file is ALREADY loaded, promote synchronously this
        // frame — no need to wait for the next drive tick.
        if let Some(entry) = file_cache.peek(&path) {
            self.entries.insert(
                qualified.clone(),
                ClassStatus::Ready(Arc::new(CachedClass {
                    qualified: qualified.clone(),
                    source: Arc::clone(&entry.source),
                    ast: Arc::clone(&entry.ast),
                    syntax: Arc::clone(&entry.syntax),
                    file_path: entry.path.clone(),
                })),
            );
            return true;
        }
        // Otherwise ask `FileCache` and remember our binding.
        file_cache.request(path.clone());
        self.entries.insert(qualified, ClassStatus::PendingFile(path));
        true
    }
}

/// Bevy system: for each class entry waiting on its file, check
/// whether `FileCache` has it now. If yes, promote to Ready. If
/// the file load failed, propagate the failure.
pub fn drive_class_cache(
    mut classes: Option<ResMut<ClassCache>>,
    files: Option<Res<FileCache>>,
) {
    let (Some(classes), Some(files)) = (classes.as_mut(), files.as_ref()) else {
        return;
    };
    // Snapshot pending keys → paths so we can mutate `entries` below.
    let pending: Vec<(String, PathBuf)> = classes
        .entries
        .iter()
        .filter_map(|(q, s)| match s {
            ClassStatus::PendingFile(p) => Some((q.clone(), p.clone())),
            _ => None,
        })
        .collect();
    for (qualified, path) in pending {
        if let Some(entry) = files.peek(&path) {
            classes.entries.insert(
                qualified.clone(),
                ClassStatus::Ready(Arc::new(CachedClass {
                    qualified: qualified.clone(),
                    source: Arc::clone(&entry.source),
                    ast: Arc::clone(&entry.ast),
                    syntax: Arc::clone(&entry.syntax),
                    file_path: entry.path.clone(),
                })),
            );
            info!("[ClassCache] promoted `{}` (file hit)", qualified);
            continue;
        }
        // File failed? Propagate.
        if let Some(lunco_cache::ResourceState::Failed(msg)) =
            files.0.state(&path)
        {
            classes
                .entries
                .insert(qualified.clone(), ClassStatus::Failed(Arc::clone(msg)));
            warn!(
                "[ClassCache] `{}` failed because file `{}` failed: {}",
                qualified,
                path.display(),
                msg
            );
        }
    }
}

/// Helper for non-system callers (Bevy commands, render functions)
/// to kick a class load without plumbing both `ResMut`s at every
/// call site. Takes `&mut World` so it can fetch both resources.
///
/// Returns whether a new load was started.
pub fn request_class(world: &mut World, qualified: impl AsRef<str>) -> bool {
    let qualified = qualified.as_ref().to_string();
    // Two-step borrow: get path + file state first (immutable/scoped),
    // then mutate class + file caches together. We can't hold two
    // mutable resource borrows simultaneously via `world.resource_mut`,
    // so funnel through `ResourceScope`.
    world.resource_scope::<ClassCache, bool>(|world, mut classes| {
        let Some(mut files) = world.get_resource_mut::<FileCache>() else {
            return false;
        };
        classes.request(qualified, &mut files)
    })
}

// ═══════════════════════════════════════════════════════════════════
// Qualified name → file path resolution (static index)
// ═══════════════════════════════════════════════════════════════════

pub fn msl_class_to_file_index(
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
    // been fetched + decompressed (see `msl_component_library` for the
    // same trick). If we'd `OnceLock::set` an empty map here, the
    // index would stay empty for the lifetime of the page even after
    // MSL lands. So: return an empty placeholder *without* memoising,
    // so the next caller retries the build.
    let lib = crate::visual_diagram::msl_component_library();
    if lib.is_empty() {
        return EMPTY.get_or_init(std::collections::HashMap::new);
    }
    INDEX.get_or_init(build_msl_class_to_file_index)
}

fn build_msl_class_to_file_index(
) -> std::collections::HashMap<String, std::path::PathBuf> {
    let start = web_time::Instant::now();
    let lib = crate::visual_diagram::msl_component_library();
    let mut map = std::collections::HashMap::with_capacity(lib.len());
    for comp in lib {
        if let Some(path) = locate_msl_file(&comp.msl_path) {
            map.insert(comp.msl_path.clone(), path);
        }
    }
    info!(
        "[ClassCache] MSL class index built: {} classes in {:?}",
        map.len(),
        start.elapsed()
    );
    map
}

pub fn locate_msl_file(qualified: &str) -> Option<std::path::PathBuf> {
    let segments: Vec<&str> = qualified.split('.').collect();
    if segments.is_empty() {
        return None;
    }

    // 1. In-memory bundle — populated on web by `MslRemotePlugin`.
    //    Returns relative paths (e.g. `Modelica/Blocks/package.mo`).
    //    `parse_msl_file_cached` re-keys against `GLOBAL_PARSED_MSL`
    //    using the same forward-slash relative path.
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

pub fn resolve_msl_class_path(qualified: &str) -> Option<std::path::PathBuf> {
    msl_class_to_file_index().get(qualified).cloned()
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
// and `locate_msl_file` can't find `Rotational/` at MSL root, we
// look `Rotational` up in `by_head`, prefix-rewrite, retry.
//
// Each entry here is a *package container* — a directory with
// `package.mo` or a flat `.mo` file immediately under some parent.
// Classes nested *inside* `.mo` files (e.g. `Modelica.Units.SI`
// lives inside `Modelica/Units.mo`) don't appear as filesystem
// entries; those still need either explicit user imports or a
// loaded-file import-scope scan.

#[derive(Debug, Default)]
pub struct MslFsIndex {
    /// Last-segment → full qualified names. `"Rotational"` may map
    /// to multiple fully-qualified packages; resolver tries each.
    pub by_head: std::collections::HashMap<String, Vec<String>>,
    /// Full qualified name → on-disk file.
    pub qualified_to_path: std::collections::HashMap<String, std::path::PathBuf>,
}

pub fn msl_fs_index() -> &'static MslFsIndex {
    use std::sync::OnceLock;
    static INDEX: OnceLock<MslFsIndex> = OnceLock::new();
    INDEX.get_or_init(build_msl_fs_index)
}

fn build_msl_fs_index() -> MslFsIndex {
    let start = web_time::Instant::now();
    let Some(root) = lunco_assets::msl_source_root_path() else {
        return MslFsIndex::default();
    };
    let mut index = MslFsIndex::default();
    walk_msl_fs(&root, &root, &[], &mut index);
    info!(
        "[ClassCache] MSL fs index built: {} qualified paths, {} distinct heads in {:?}",
        index.qualified_to_path.len(),
        index.by_head.len(),
        start.elapsed()
    );
    index
}

fn walk_msl_fs(
    root: &std::path::Path,
    dir: &std::path::Path,
    prefix: &[String],
    index: &mut MslFsIndex,
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
            walk_msl_fs(root, &path, &next, index);
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
pub fn resolve_msl_head_prefix(qualified: &str) -> Option<String> {
    // Direct hit first — head is already at MSL root.
    if locate_msl_file(qualified).is_some() {
        return Some(qualified.to_string());
    }
    let (head, rest) = qualified.split_once('.').unwrap_or((qualified, ""));
    let index = msl_fs_index();
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
        let candidate = if rest.is_empty() {
            full_head.clone()
        } else {
            format!("{full_head}.{rest}")
        };
        if locate_msl_file(&candidate).is_some() {
            return Some(candidate);
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════
// Sync MSL class loader — for callers that need a class *right now*
// ═══════════════════════════════════════════════════════════════════

/// Synchronously resolve an MSL class by qualified name and return
/// its [`ClassDef`]. Lazily reads + parses the containing `.mo` file
/// and memoises the result by qualified name.
///
/// # Why sync (vs. the main `ClassCache.request` async flow)
///
/// The async [`ClassCache`] is the right tier for foreground UI
/// loads — user clicks Drill-in, a background task parses, the
/// canvas re-projects next frame. But *icon extraction* runs inside
/// the projector pipeline where we need the parent-class AST NOW to
/// resolve `extends`-graphics inheritance. Deferring by a frame
/// means rendering every MSL sensor / partial without its inherited
/// body on first open, then popping in — bad UX.
///
/// This helper does the blocking I/O + parse once per qualified name
/// and caches the `Arc<ClassDef>` for instant subsequent calls. MSL
/// files are small (most ≤ a few KB; the package-aggregate files are
/// at worst a few hundred KB) so a one-shot read is acceptable. Any
/// resolution failure is memoised as `None` so repeated hits don't
/// re-hammer the filesystem.
///
/// Used by the icon-inheritance resolver so `SpeedSensor extends
/// Modelica.Mechanics.Rotational.Icons.RelativeSensor` pulls in the
/// parent's rectangle/text primitives the first time it renders.
/// Process-wide [`ModelicaEngine`] holding every MSL class that's
/// been touched in this session. Replaces the prior parallel
/// `HashMap<qualified, Arc<ClassDef>>` — the engine's rumoca session
/// IS the cache. Misses load the file from
/// [`lunco_assets::msl::global_msl_source`] (filesystem on native,
/// in-memory bundle on web) and feed it into the session via
/// `add_document`; subsequent lookups hit rumoca's per-file
/// fingerprint cache.
fn msl_engine() -> &'static std::sync::Mutex<crate::engine::ModelicaEngine> {
    use std::sync::{Mutex, OnceLock};
    static ENGINE: OnceLock<Mutex<crate::engine::ModelicaEngine>> = OnceLock::new();
    ENGINE.get_or_init(|| Mutex::new(crate::engine::ModelicaEngine::new()))
}

/// Read the source bytes for an MSL relative path, going through the
/// process-wide [`MslAssetSource`]. Returns `None` if the source
/// hasn't been installed yet (web boot before fetch completes) or
/// the path isn't present.
fn read_msl_source_bytes(path: &std::path::Path) -> Option<String> {
    let source = lunco_assets::msl::global_msl_source()?;
    let bytes = source.read(path)?;
    String::from_utf8(bytes).ok()
}

pub fn peek_or_load_msl_class(
    qualified: &str,
) -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
    let mut engine = msl_engine().lock().ok()?;
    if !engine.has_class(qualified) {
        let path = resolve_msl_class_path(qualified).or_else(|| locate_msl_file(qualified))?;
        let source = read_msl_source_bytes(&path)?;
        let uri = path.to_string_lossy().replace('\\', "/");
        engine.session_mut().add_document(&uri, &source).ok()?;
    }
    engine.class_def(qualified).map(Arc::new)
}

/// Non-blocking variant of [`peek_or_load_msl_class`] — returns the
/// `Arc<ClassDef>` if the engine session already holds it, and
/// `None` *without triggering a load* on a miss.
///
/// Use this from hot paths that must not block on rumoca parse —
/// notably the projection task running on Bevy's AsyncComputeTaskPool,
/// where a sync MSL parse from inside a worker that's already serving
/// a parent rumoca parse stalls for the projection deadline.
pub fn peek_msl_class_cached(
    qualified: &str,
) -> Option<Arc<rumoca_session::parsing::ast::ClassDef>> {
    let mut engine = msl_engine().lock().ok()?;
    if !engine.has_class(qualified) {
        return None;
    }
    engine.class_def(qualified).map(Arc::new)
}

// ═══════════════════════════════════════════════════════════════════
// Plugin
// ═══════════════════════════════════════════════════════════════════

pub struct ClassCachePlugin;

impl Plugin for ClassCachePlugin {
    fn build(&self, app: &mut App) {
        // Do NOT redirect `RUMOCA_CACHE_DIR` here. Leaving it unset
        // lets rumoca use its XDG default (`~/.cache/rumoca/...`),
        // shared with the `modelica_tester` CLI and any other
        // rumoca-using tool. The workspace-local override that
        // used to live here made the workbench re-parse the entire
        // MSL (~2670 files, minutes) after every rumoca source
        // change because the artifact-cache key schema invalidated,
        // while the CLI kept hitting its XDG cache. See
        // `ModelicaPlugin::build_modelica_core` for the matching
        // comment.

        app.init_resource::<FileCache>()
            .init_resource::<ClassCache>()
            // FileCache drives FIRST so newly-finished files are
            // visible to ClassCache's promoter on the same frame.
            .add_systems(Update, (drive_file_cache, drive_class_cache).chain());
    }
}
