//! `ModelicaDocument` — the Document System representation of one `.mo` file.
//!
//! # Canonicality: source text, AST cached
//!
//! The Document owns the **source text** as its canonical state. Text is what
//! the user types, what lives on disk, and what preserves comments + formatting
//! losslessly — the things both a human code editor and an AI `Edit` tool
//! depend on.
//!
//! Alongside the text, the Document caches a **parsed AST**
//! ([`AstCache`]). The cache is refreshed eagerly after every mutation so
//! panels that need structural access (diagram, parameter inspector,
//! placement extractor) can read `doc.ast()` without reparsing. Parse
//! failures are observable via [`AstCache::result`] — the cache is always
//! present, but it may hold an error.
//!
//! Documents are keyed by [`lunco_doc::DocumentId`] inside
//! [`ui::ModelicaDocumentRegistry`]. Every place that spawns a
//! `ModelicaModel` entity allocates a document in the registry and writes
//! its id into [`crate::ModelicaModel::document`].
//!
//! # Op set
//!
//! Text-level ops (comfortable for human editors and AI text tools):
//!
//! - [`ModelicaOp::ReplaceSource`] — coarse full-buffer swap. Used by
//!   CodeEditor's Compile and by any caller that produces the whole new
//!   source (e.g. template expansion).
//! - [`ModelicaOp::EditText`] — byte-range replacement. Used for granular
//!   text edits that should participate in undo/redo without losing
//!   precision.
//!
//! AST-level ops (planned for Task 4) will splice text via AST-node spans
//! so structural edits from the diagram / parameter panels land as
//! minimal text diffs, preserving surrounding formatting and comments.
//!
//! Inverses: [`ReplaceSource`](ModelicaOp::ReplaceSource)'s inverse carries
//! the previous full source. [`EditText`](ModelicaOp::EditText)'s inverse
//! is another `EditText` against the *new* range with the previous slice
//! as replacement.

use std::collections::VecDeque;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lunco_doc::{Document, DocumentError, DocumentId, DocumentOp, DocumentOrigin};
use rumoca_phase_parse::parse_to_syntax;
use rumoca_session::parsing::ast::StoredDefinition;

use crate::pretty::{self, ComponentDecl, ConnectEquation, Placement, PortRef};

/// How many structured changes the document retains for consumer
/// polling. Consumers (panels, worker threads) track the last
/// generation they observed and pull the tail of this buffer.
/// When a consumer falls further behind than this window, they must
/// do a full rebuild from the current AST.
///
/// 256 is enough for typical interactive sessions (hundreds of
/// drag+drop + parameter edits between panel refreshes). Tuned up
/// when profiling shows panel consumers lagging.
pub const CHANGE_HISTORY_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// ModelicaChange — structured change events for incremental patching
// ---------------------------------------------------------------------------

/// A structural change to a [`ModelicaDocument`].
///
/// Emitted on every successful mutation and retained in a bounded ring
/// buffer so panels can patch their render state incrementally rather
/// than rebuilding from the AST on every frame / every edit.
///
/// Text-level ops ([`ModelicaOp::EditText`], [`ModelicaOp::ReplaceSource`])
/// emit [`Self::TextReplaced`] because they can't be losslessly
/// projected onto structural changes; consumers handle that variant by
/// doing a full rebuild. All AST-level ops emit their specific variant,
/// giving consumers enough info to patch node lists / edge lists /
/// placement maps without parsing anything themselves.
///
/// Undo and redo propagate as `TextReplaced` — structural changes are
/// only emitted on *original* forward application of a structural op.
/// Panels that want structural undo/redo can diff the AST themselves
/// when they see `TextReplaced`, or rebuild.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ModelicaChange {
    /// Text-level change or undo/redo. Consumers must rebuild state
    /// that tracks structural entities (components, connections).
    TextReplaced,
    /// A component was added to `class`.
    ComponentAdded {
        /// Qualified class name (supports dotted nested paths).
        class: String,
        /// Instance name of the new component.
        name: String,
    },
    /// A component was removed from `class`.
    ComponentRemoved {
        /// Qualified class name.
        class: String,
        /// Instance name that was removed.
        name: String,
    },
    /// A connect equation was added to `class`'s equation section.
    ConnectionAdded {
        /// Qualified class name.
        class: String,
        /// Source port.
        from: PortRef,
        /// Target port.
        to: PortRef,
    },
    /// A connect equation was removed from `class`'s equation section.
    ConnectionRemoved {
        /// Qualified class name.
        class: String,
        /// Source port.
        from: PortRef,
        /// Target port.
        to: PortRef,
    },
    /// A component's `Placement` annotation was set or replaced.
    PlacementChanged {
        /// Qualified class name.
        class: String,
        /// Component instance name.
        component: String,
        /// The placement now in effect.
        placement: Placement,
    },
    /// A component's parameter modification was set or replaced.
    ParameterChanged {
        /// Qualified class name.
        class: String,
        /// Component instance name.
        component: String,
        /// Parameter name.
        param: String,
        /// Replacement value expression (emitted verbatim).
        value: String,
    },
    /// A class was added (long form via [`ModelicaOp::AddClass`] or short
    /// form via [`ModelicaOp::AddShortClass`]). `qualified` is the fully
    /// qualified path (`parent.name` or just `name` for top-level).
    ClassAdded {
        /// Fully-qualified class name.
        qualified: String,
        /// Class kind keyword (`model`, `block`, `connector`, ...).
        kind: pretty::ClassKindSpec,
    },
    /// A class was removed.
    ClassRemoved {
        /// Fully-qualified class name that no longer exists.
        qualified: String,
    },
}

/// Single parse cache attached to a [`ModelicaDocument`].
///
/// Lenient parser (rumoca's `parse_to_syntax`) always produces a
/// best-effort `StoredDefinition`. `errors` carries any diagnostics
/// rumoca's recovery emitted; empty when source is well-formed.
///
/// Replaces the previous `AstCache` (strict, Result-shaped) +
/// `SyntaxCache` (lenient, AST-shaped) pair. The two parsers ran the
/// same source through rumoca twice on every edit; for valid sources
/// they produced byte-identical ASTs, and for invalid sources only
/// strict actually rejected — but `parse_to_syntax` already exposes
/// the same error list, so the strict parse was redundant. Folding
/// into one type halves the parse cost and removes the dual-staleness
/// state-machine the readers had to keep coherent.
///
/// Old name `SyntaxCache` is kept as a type alias so the migration
/// doesn't have to touch every reader at once. Old `AstCache` is
/// removed; sites that read `cache.result` migrate to
/// `cache.has_errors()` / `cache.errors`.
#[derive(Debug, Clone)]
pub struct SyntaxCache {
    /// Document generation at which this cache was produced.
    pub generation: u64,
    /// Best-effort parsed AST. Always present, even when the source
    /// has parse errors — rumoca's lenient parser returns whatever it
    /// could recover.
    pub ast: Arc<StoredDefinition>,
    /// Diagnostic strings from the parse. Empty when the source is
    /// well-formed. UIs that show a "broken file" badge read
    /// `has_errors()`; the diagnostics panel shows each entry as a
    /// row.
    pub errors: Vec<String>,
}

/// Back-compat alias for the removed strict-parse cache. Existing
/// callers reading `doc.ast()` keep working — both methods now return
/// the same single `SyntaxCache`.
pub type AstCache = SyntaxCache;

impl SyntaxCache {
    /// Empty placeholder cache. Used at document construction on
    /// wasm so the heavy rumoca parse can be ran by the off-thread
    /// worker and its result backfilled here later.
    pub fn empty(generation: u64) -> Self {
        Self {
            generation,
            ast: Arc::new(StoredDefinition::default()),
            errors: Vec::new(),
        }
    }

    /// Parse `source` into a fresh cache at the given generation.
    /// Wasm path returns an empty placeholder; the worker parse fills
    /// it via [`Self::install_from_worker`].
    pub fn from_source(source: &str, generation: u64) -> Self {
        if std::env::var_os("LUNCO_NO_PARSE").is_some() {
            return Self::empty(generation);
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = source;
            return Self::empty(generation);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let recovery = parse_to_syntax(source, "model.mo");
            let errors = recovery
                .parse_errors()
                .iter()
                .map(|e| format!("{e:?}"))
                .collect();
            let ast = Arc::new(recovery.best_effort().clone());
            Self {
                generation,
                ast,
                errors,
            }
        }
    }

    /// Install a worker-parsed AST + errors. Replaces the
    /// previously-empty placeholder once the off-thread parse lands.
    pub fn install_from_worker(
        &mut self,
        ast: Arc<StoredDefinition>,
        errors: Vec<String>,
    ) {
        self.ast = ast;
        self.errors = errors;
    }

    /// `true` when the parse emitted any diagnostics. Source might
    /// still have produced a partial AST — readers walk `ast`
    /// regardless and use this flag for the "broken file" badge.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// First diagnostic, if any. Convenience for "Err-shaped" old
    /// callsites that wanted a single message string.
    pub fn first_error(&self) -> Option<&str> {
        self.errors.first().map(|s| s.as_str())
    }

    /// Shortcut: the salvaged AST.
    pub fn ast(&self) -> &StoredDefinition {
        &self.ast
    }
}

/// The canonical Document representation of one Modelica source file.
///
/// Owns the source text + a [`DocumentOrigin`] describing where it
/// came from (which drives save behavior, tab title, read-only
/// badge) + a parsed-AST cache ([`AstCache`]) refreshed eagerly after
/// every mutation.
#[derive(Debug, Clone)]
pub struct ModelicaDocument {
    id: DocumentId,
    source: String,
    /// Single parse cache (lenient). Replaces the old AstCache /
    /// SyntaxCache duo — they redundantly held the same lenient AST
    /// after the strict parser was retired. See [`SyntaxCache`] for
    /// the full rationale.
    syntax: Arc<SyntaxCache>,
    /// UI-projection of the current AST. Rebuilt from `syntax` on every
    /// successful parse install. Panels read this instead of the AST
    /// directly. See `crate::index::ModelicaIndex`.
    index: crate::index::ModelicaIndex,
    generation: u64,
    origin: DocumentOrigin,
    /// Generation at which the document was last persisted to disk.
    /// `None` means never saved (freshly created in-memory); `Some(g)`
    /// means last saved at generation `g`. See [`is_dirty`](Self::is_dirty).
    last_saved_generation: Option<u64>,
    /// Ring buffer of `(generation_after_change, change)`. The oldest
    /// entry is dropped when `CHANGE_HISTORY_CAPACITY` is exceeded.
    /// Consumers track their last-observed generation and pull the
    /// suffix via [`changes_since`](Self::changes_since).
    changes: VecDeque<(u64, ModelicaChange)>,
    /// When the source was last mutated (for AST-reparse debouncing).
    /// Set by `apply_patch`; read by
    /// [`refresh_stale_asts`](../ui/fn.refresh_stale_asts.html) which
    /// reparses only after a quiet period so rapid typing stays
    /// responsive. `None` for fresh docs whose source hasn't changed
    /// since construction.
    last_source_edit_at: Option<web_time::Instant>,
}

impl ModelicaDocument {
    /// Build an in-memory `ModelicaDocument` with the given name as
    /// its Untitled identifier. Starts dirty (never-saved).
    pub fn new(id: DocumentId, source: impl Into<String>) -> Self {
        Self::with_origin(
            id,
            source,
            DocumentOrigin::untitled(format!("Untitled-{}", id.raw())),
        )
    }

    /// Build a `ModelicaDocument` with an explicit origin.
    ///
    /// For on-disk origins (read-only library entries, writable user
    /// files) the source is assumed to match disk at generation 0, so
    /// the document starts clean. Untitled origins start dirty.
    /// Build a `ModelicaDocument` with an explicit origin.
    ///
    /// Lazy by design: no parse happens here. Empty `SyntaxCache`
    /// + `AstCache` carrying "parse pending"; `last_source_edit_at`
    /// stamped to now so the engine sync system parses the source
    /// on the next idle Update tick. Tab opens instantly; engine
    /// catches up off-thread.
    ///
    /// The doc-side caches are populated reactively by
    /// [`crate::engine_resource::drive_engine_sync`]'s drain step —
    /// they're a mirror of the engine session, not the source of
    /// truth. Panels can keep reading `doc.strict_ast()` /
    /// `doc.syntax_arc()`; they'll see the parsed state as soon as
    /// the engine completes.
    pub fn with_origin(
        id: DocumentId,
        source: impl Into<String>,
        origin: DocumentOrigin,
    ) -> Self {
        let source = source.into();
        let syntax = Arc::new(SyntaxCache::empty(0));
        let mut doc = Self::from_parts(id, source, origin, syntax);
        doc.last_source_edit_at = Some(web_time::Instant::now());
        doc.generation = 1;
        doc
    }


    /// Load a `.mo` file from disk or the in-memory MSL bundle and
    /// build a fresh document. Uses rumoca's content-hash artifact
    /// cache via `parse_files_parallel`, so subsequent loads of a
    /// file the engine session has already parsed return the same
    /// `Arc<StoredDefinition>` as a cache hit.
    ///
    /// `path` may be either an absolute filesystem path (native) or
    /// an MSL-relative URI like `Modelica/Blocks/Continuous.mo`
    /// resolved against the in-memory bundle (web). Source bytes
    /// come from `lunco_assets::msl::global_msl_source().read(...)`
    /// when available, falling back to `std::fs::read_to_string`.
    ///
    /// This is the canonical async-friendly loader: spawn it inside
    /// an `AsyncComputeTaskPool::spawn` and install the result via
    /// `ModelicaDocumentRegistry::install_prebuilt` on the main
    /// thread. The drill-in flow uses exactly this pattern.
    /// Load just the target class out of an MSL package file,
    /// instead of the whole wrapper. Drill-in path uses this so
    /// opening `Modelica.Blocks.Examples.PID_Controller` yields a
    /// ~7 KB doc (the PID class with leading comments) instead of
    /// the 152 KB `Modelica/Blocks/package.mo` wrapper.
    ///
    /// Source layout: `within Modelica.Blocks.Examples;\n<class_slice>`.
    /// The within prefix preserves scope-chain resolution for
    /// in-class references that rely on the parent package
    /// (`SI.Angle`, `Blocks.Math.Gain`, etc.).
    ///
    /// The doc is **lazy**: AST/SyntaxCache start empty, marked stale
    /// so `drive_engine_sync` parses the small slice off-thread. Tab
    /// opens immediately; content fills on the next idle Update.
    ///
    /// `qualified` is the MSL fully-qualified class name (e.g.
    /// `Modelica.Blocks.Examples.PID_Controller`); `path` is the
    /// `.mo` file containing that class (typically a `package.mo`).
    pub fn load_msl_class(
        id: DocumentId,
        path: &std::path::Path,
        qualified: &str,
    ) -> Result<Self, String> {
        // Read whole-file bytes (in-memory bundle on web, fs on native).
        let full_source = if let Some(bytes) = lunco_assets::msl::global_msl_source()
            .and_then(|s| s.read(path))
        {
            String::from_utf8(bytes)
                .map_err(|e| format!("non-utf8 source `{}`: {e}", path.display()))?
        } else {
            std::fs::read_to_string(path).map_err(|e| {
                format!("read failed `{}`: {e}", path.display())
            })?
        };

        // Resolve the target class span via rumoca's cached parse.
        let short_name = qualified.rsplit('.').next().unwrap_or(qualified);
        let parent_pkg: String = {
            let mut parts: Vec<&str> = qualified.split('.').collect();
            parts.pop();
            parts.join(".")
        };

        // **Wasm: use the pre-parsed MSL bundle.** A synchronous
        // `parse_files_parallel` on a 100 KB MSL package file blocks
        // `AsyncComputeTaskPool` (which IS the main thread on
        // wasm32-unknown-unknown) for tens of seconds — exactly the
        // freeze users see clicking sub-classes inside a Continuous /
        // Blocks tab. The MSL bundle already has every file's AST
        // resident; look it up by path key. Native still parses
        // because it has real worker threads.
        #[cfg(target_arch = "wasm32")]
        let ast: rumoca_session::parsing::ast::StoredDefinition = {
            let key = path.to_string_lossy().to_string();
            crate::msl_remote::global_parsed_msl()
                .and_then(|b| b.iter().find(|(k, _)| k == &key).map(|(_, a)| a.clone()))
                .ok_or_else(|| format!(
                    "load_msl_class: pre-parsed AST missing for `{key}` \
                     (MSL bundle not yet ready or path mismatch)"
                ))?
        };
        #[cfg(not(target_arch = "wasm32"))]
        let ast: rumoca_session::parsing::ast::StoredDefinition = {
            let mut parsed = rumoca_session::parsing::parse_files_parallel(&[path.to_path_buf()])
                .map_err(|e| format!("parse failed `{}`: {e}", path.display()))?;
            let (_uri, ast) = parsed
                .drain(..)
                .next()
                .ok_or_else(|| format!("rumoca returned no parse result for `{}`", path.display()))?;
            ast
        };

        let class_def = find_class_by_short_name_recursive(&ast, short_name)
            .ok_or_else(|| format!("class `{qualified}` not found in `{}`", path.display()))?;
        let (full_start, full_end) = class_def
            .full_span_with_leading_comments(&full_source)
            .ok_or_else(|| format!("could not slice class `{qualified}` from `{}`", path.display()))?;
        let class_slice = &full_source[full_start..full_end];

        // Compose final source: within prefix + class slice. Empty
        // parent (top-level class) drops the within line.
        let source = if parent_pkg.is_empty() {
            class_slice.to_string()
        } else {
            format!("within {parent_pkg};\n{class_slice}")
        };

        let origin = lunco_doc::DocumentOrigin::File {
            path: path.to_path_buf(),
            writable: false,
        };
        // Lazy: SyntaxCache empty, stale. drive_engine_sync's async
        // parse fills it on next Update tick. Tab paints immediately
        // with a "Loading…" overlay until the parse lands.
        Ok(Self::with_origin(id, source, origin))
    }

    pub fn load_msl_file(
        id: DocumentId,
        path: &std::path::Path,
    ) -> Result<Self, String> {
        // 1. Source bytes — try the in-memory bundle first (web),
        //    then fall back to a filesystem read (native). Mirrors
        //    `class_cache::read_msl_source_bytes`.
        let source = if let Some(bytes) = lunco_assets::msl::global_msl_source()
            .and_then(|s| s.read(path))
        {
            String::from_utf8(bytes)
                .map_err(|e| format!("non-utf8 source `{}`: {e}", path.display()))?
        } else {
            std::fs::read_to_string(path).map_err(|e| {
                format!("read failed `{}`: {e}", path.display())
            })?
        };

        // 2. Strict parse via rumoca-session's content-hash artifact
        //    cache — re-opening a file the engine session has parsed
        //    is essentially free. The strict `StoredDefinition` flows
        //    into the lenient `SyntaxCache` (single canonical local
        //    Arc); `AstCache` records only success/failure.
        //
        // Wasm: `parse_files_parallel` runs synchronously on the
        // calling thread which on wasm32-unknown-unknown is the main
        // thread (AsyncComputeTaskPool is cooperative). Re-parsing a
        // 100 KB MSL file freezes the UI for tens of seconds. Look up
        // the AST in the pre-parsed bundle instead.
        let parsed: Result<Arc<StoredDefinition>, String> =
            if std::env::var_os("LUNCO_NO_PARSE").is_some() {
                Err("LUNCO_NO_PARSE diagnostic — parse skipped".into())
            } else {
                #[cfg(target_arch = "wasm32")]
                {
                    let key = path.to_string_lossy().to_string();
                    crate::msl_remote::global_parsed_msl()
                        .and_then(|b| b.iter()
                            .find(|(k, _)| k == &key)
                            .map(|(_, a)| Arc::new(a.clone())))
                        .ok_or_else(|| format!(
                            "load_msl_file: pre-parsed AST missing for `{key}` \
                             (MSL bundle not yet ready or path mismatch)"
                        ))
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    match rumoca_session::parsing::parse_files_parallel(&[path.to_path_buf()]) {
                        Ok(mut pairs) if !pairs.is_empty() => {
                            let (_, stored) = pairs.remove(0);
                            Ok(Arc::new(stored))
                        }
                        Ok(_) => Err("rumoca returned no parse result".into()),
                        Err(e) => Err(e.to_string()),
                    }
                }
            };

        // Single canonical parse cache. Strict success → adopt the
        // parsed `StoredDefinition`, errors empty. Strict failure →
        // empty AST + the rumoca diagnostic surfaced into `errors`,
        // matching what `parse_to_syntax` would have produced. The
        // doc's edit-driven refresh fills in lenient salvage on the
        // next Update if the user keeps editing.
        let syntax = Arc::new(match parsed {
            Ok(strict) => SyntaxCache {
                generation: 0,
                ast: strict,
                errors: Vec::new(),
            },
            Err(msg) => SyntaxCache {
                generation: 0,
                ast: Arc::new(StoredDefinition::default()),
                errors: vec![msg],
            },
        });

        let origin = lunco_doc::DocumentOrigin::File {
            path: path.to_path_buf(),
            writable: false,
        };
        Ok(Self::from_parts(id, source, origin, syntax))
    }

    /// Build a document from pre-computed parts. Skips the rumoca
    /// parse — callers must supply a [`SyntaxCache`] whose
    /// `generation` is `0`. Used by the class cache to avoid
    /// re-parsing every time a tab binds to a known class.
    pub fn from_parts(
        id: DocumentId,
        source: String,
        origin: DocumentOrigin,
        syntax: Arc<SyntaxCache>,
    ) -> Self {
        debug_assert_eq!(
            syntax.generation, 0,
            "from_parts expects a freshly-parsed SyntaxCache"
        );
        let last_saved_generation = if origin.is_untitled() {
            None
        } else {
            Some(0)
        };
        let has_errors = syntax.has_errors();
        let mut index = crate::index::ModelicaIndex::new();
        index.rebuild_with_errors(&syntax.ast, &source, has_errors);
        Self {
            id,
            source,
            syntax,
            index,
            generation: 0,
            origin,
            last_saved_generation,
            changes: VecDeque::with_capacity(CHANGE_HISTORY_CAPACITY),
            last_source_edit_at: None,
        }
    }

    /// Read-only access to the per-document UI projection. Panels read
    /// this instead of touching the AST directly. The Index is rebuilt
    /// on every successful parse install; between installs (during
    /// rapid typing) it lags the source — same staleness contract as
    /// the AST cache itself.
    pub fn index(&self) -> &crate::index::ModelicaIndex {
        &self.index
    }

    /// The current source text.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Back-compat accessor for the parse cache. After the dual-cache
    /// collapse this returns the same single `SyntaxCache` as
    /// [`Self::syntax`]; callers haven't yet been migrated. Old code
    /// reading `cache.result` should switch to `cache.has_errors()` /
    /// `cache.first_error()`.
    pub fn ast(&self) -> &SyntaxCache {
        &self.syntax
    }

    /// Returns `true` when the source has been edited since the last
    /// parse. Same as [`Self::syntax_is_stale`]; alias kept while
    /// callers migrate.
    pub fn ast_is_stale(&self) -> bool {
        self.syntax.generation != self.generation
    }

    /// Wall-clock time of the last source mutation, or `None` if the
    /// document hasn't been edited since construction. Used by the
    /// debounce driver to determine whether the typing "quiet
    /// window" has elapsed.
    pub fn last_source_edit_at(&self) -> Option<web_time::Instant> {
        self.last_source_edit_at
    }

    /// Backdate `last_source_edit_at` past the debounce window so the
    /// next [`crate::engine_resource::drive_engine_sync`] tick spawns
    /// an AST parse immediately, without waiting for the typing-debounce.
    /// Use this after a structured / API edit (one discrete commit)
    /// where the debounce — which exists to coalesce keystroke bursts
    /// — only adds latency. Has no effect when the AST is already fresh.
    pub fn waive_ast_debounce(&mut self) {
        if self.last_source_edit_at.is_some() {
            let backdate_ms =
                (crate::engine_resource::AST_DEBOUNCE_MS as u64).saturating_add(1);
            self.last_source_edit_at = Some(
                web_time::Instant::now() - std::time::Duration::from_millis(backdate_ms),
            );
        }
    }

    /// The cached lenient parse. Always present, may be stale —
    /// same staleness contract as [`Self::ast`]. Browser, outline,
    /// and any panel that must keep rendering through partial-parse
    /// states reads this; UI is the only consumer of `has_errors`.
    pub fn syntax(&self) -> &SyntaxCache {
        &self.syntax
    }

    /// The `Arc` backing [`Self::syntax`]. Use this when you need to
    /// hand the cache to another thread or hold it across a registry
    /// borrow without keeping the document borrowed.
    pub fn syntax_arc(&self) -> &Arc<SyntaxCache> {
        &self.syntax
    }

    /// The current strict-parsed AST as an `Arc<StoredDefinition>`,
    /// `None` when the strict parse failed.
    ///
    /// Vends the lenient [`SyntaxCache`]'s `Arc` — for valid sources
    /// strict and lenient produce the same AST, so we share storage.
    /// Returns `None` when `ast.result` is `Err`, signalling the source
    /// has hard parse errors and callers (compile, codegen) should
    /// not proceed.
    ///
    /// Replaces the previous `doc.ast().result.as_ref().ok().cloned()`
    /// pattern. The strict `Arc<StoredDefinition>` no longer lives in
    /// [`AstCache`] — engine + lenient cache are the canonical
    /// storage.
    pub fn strict_ast(&self) -> Option<Arc<StoredDefinition>> {
        if !self.syntax.has_errors() {
            Some(Arc::clone(&self.syntax.ast))
        } else {
            None
        }
    }

    /// Returns `true` when the lenient parse is behind the current
    /// source generation. Mirrors [`Self::ast_is_stale`].
    pub fn syntax_is_stale(&self) -> bool {
        self.syntax.generation != self.generation
    }

    /// Install the parse cache from an off-thread refresh pass. The
    /// cache's `generation` must match `self.generation` — otherwise
    /// the source has moved on while parsing was in flight and the
    /// result is stale (the next debounce will kick a fresh parse).
    ///
    /// The two-arg `(ast, syntax)` form pre-collapse is gone; one
    /// `SyntaxCache` carries both pieces of information.
    pub fn install_parse_results(&mut self, syntax: SyntaxCache) {
        if syntax.generation != self.generation {
            return;
        }
        self.syntax = Arc::new(syntax);
        self.rebuild_index();
        self.last_source_edit_at = None;
    }

    /// Rebuild the UI projection [`Index`](crate::index::ModelicaIndex)
    /// from the **engine's** view of this document.
    ///
    /// Contract (Shape C of the engine-as-Index migration): the per-doc
    /// `Index` is a derived projection over the workspace
    /// [`crate::engine::ModelicaEngine`] session, with the per-doc
    /// optimistic-patch overlay layered on top. To keep that contract
    /// honest, we upsert this doc's current source into the engine
    /// before reading the parsed AST back — guaranteeing the Index is
    /// computed against engine-canonical state, not just a transient
    /// local parse that might disagree with what cross-doc queries
    /// see.
    ///
    /// Rumoca's content-hash artifact cache makes the upsert O(1) for
    /// unchanged source (the common case: install_parse_results just
    /// produced the same StoredDef the engine already has).
    ///
    /// **Fallback**: when the global engine handle isn't installed
    /// yet (early boot before `ModelicaEnginePlugin::build`, or
    /// headless tests that don't add the plugin), we fall back to
    /// rebuilding from the local `SyntaxCache::ast`. Behaviour is
    /// equivalent for in-doc fields because the engine-canonical AST
    /// for a single doc is the same parse result as the local one.
    fn rebuild_index(&mut self) {
        // Engine-free: read the local SyntaxCache directly. The
        // SyntaxCache IS the engine-mirror (populated only by
        // `drive_engine_sync`'s drain step or by `load_msl_file`'s
        // strict-adopt) — and it's `Arc<StoredDefinition>`, so the
        // walk is a pointer dereference, not a re-parse.
        //
        // Previously this site locked the engine and called
        // `upsert_document(self.id, &self.source)` — a synchronous
        // parse on the main thread. For drill-in into a 152 KB
        // MSL package that froze the workbench for 100+ seconds.
        // Removed: engine catches up async; if doc.syntax lags
        // engine by one tick, the next drain re-calls
        // `install_parse_results` which re-runs `rebuild_index`
        // with the fresh AST.
        self.index.rebuild_with_errors(
            &self.syntax.ast,
            &self.source,
            self.syntax.has_errors(),
        );
    }

    /// A clone of the source text (used by the off-thread parse task).
    /// Cheap: `String::clone` does a heap copy but for our typical
    /// 20-100 KB sources this is microseconds — negligible vs the
    /// multi-second parse it kicks off.
    pub fn source_snapshot(&self) -> String {
        self.source.clone()
    }

    /// Force an immediate strict + lenient reparse and mark both
    /// results as matching the current source generation. Callers
    /// that must see a fresh parse (Compile, Format Document, any
    /// path that hands the AST to rumoca-sim) use this before reading
    /// [`Self::ast`] or [`Self::syntax`].
    ///
    /// Idempotent — a no-op when both caches are already at the
    /// current generation.
    pub fn refresh_ast_now(&mut self) {
        if !self.ast_is_stale() && !self.syntax_is_stale() {
            return;
        }
        // Engine is the only AST source after Phase 4. If it isn't
        // installed (early boot, headless test that didn't add the
        // plugin) the doc stays at its current cache and the caller
        // sees stale data — `ModelicaEnginePlugin::build` runs
        // before any UI tick, so this branch is unreachable in
        // production.
        let Some(handle) = crate::engine_resource::global_engine_handle() else {
            bevy::log::warn!(
                "[Doc] refresh_ast_now: engine not installed; skipping reparse for doc={}",
                self.id.raw(),
            );
            return;
        };
        let t = web_time::Instant::now();
        let bytes = self.source.len();
        // Pre-parsed MSL bundle short-circuit. If this doc came from
        // a file in the MSL bundle, the parsed AST is already in
        // `crate::msl_remote::global_parsed_msl()` — call directly,
        // skip the synchronous rumoca parse (which would freeze the
        // UI for minutes on a 150 KB MSL file on wasm).
        let bundle_ast: Option<Arc<StoredDefinition>> = match &self.origin {
            DocumentOrigin::File { path, .. } => {
                let key = path.to_string_lossy().to_string();
                crate::msl_remote::global_parsed_msl().and_then(|b| {
                    b.iter()
                        .find(|(k, _)| k == &key)
                        .map(|(_, ast)| Arc::new(ast.clone()))
                })
            }
            _ => None,
        };
        // Route through engine: install the cached AST when
        // available. On wasm a cache miss MUST NOT trigger a
        // synchronous parse — that would freeze the UI for minutes
        // on a 150 KB MSL file. Native still parses sync (real
        // worker threads); wasm bails out and lets the worker-parse
        // pipeline catch up async on the next `drive_engine_sync`
        // tick.
        let arc_ast: Option<Arc<StoredDefinition>> = {
            let mut engine = handle.lock();
            if let Some(ast) = bundle_ast.as_ref() {
                engine.upsert_document_with_ast(self.id, (**ast).clone());
                Some(Arc::clone(ast))
            } else {
                #[cfg(target_arch = "wasm32")]
                {
                    bevy::log::debug!(
                        "[Doc] refresh_ast_now: wasm cache miss doc={} — \
                         deferring to worker (no sync parse)",
                        self.id.raw(),
                    );
                    None
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = engine.upsert_document(self.id, &self.source);
                    engine.parsed_for_doc(self.id).cloned().map(Arc::new)
                }
            }
        }; // engine lock released before rebuild_index re-acquires it
        match arc_ast {
            Some(ast) => {
                self.syntax = Arc::new(SyntaxCache {
                    generation: self.generation,
                    ast,
                    errors: Vec::new(),
                });
            }
            None => {
                // Strict parse failed inside engine — bump the
                // generation marker on a parsing-failed cache so the
                // staleness check doesn't loop. Callers that gate on
                // success read `has_errors()`.
                self.syntax = Arc::new(SyntaxCache {
                    generation: self.generation,
                    ast: Arc::new(StoredDefinition::default()),
                    errors: vec!["strict parse failed".into()],
                });
            }
        }
        self.rebuild_index();
        let elapsed_ms = t.elapsed().as_secs_f64() * 1000.0;
        if elapsed_ms > 5.0 {
            bevy::log::info!(
                "[Doc] refresh_ast_now: {bytes} bytes via engine in {elapsed_ms:.1}ms (gen={})",
                self.generation
            );
        }
        self.last_source_edit_at = None;
    }

    /// Iterate over structured changes applied since `last_seen`.
    ///
    /// Returns an iterator over `(generation, change)` pairs where
    /// `generation` is the document generation *after* the change
    /// landed. Consumers track their last-observed generation and
    /// advance it to the final yielded value after processing.
    ///
    /// Returns [`None`] when the consumer has fallen further behind
    /// than the retention window ([`CHANGE_HISTORY_CAPACITY`]) —
    /// callers must then do a full rebuild from the current AST.
    /// Consumers that have never polled can pass `0` to receive every
    /// retained change.
    pub fn changes_since(
        &self,
        last_seen: u64,
    ) -> Option<impl Iterator<Item = &(u64, ModelicaChange)>> {
        if let Some((earliest, _)) = self.changes.front() {
            // If the earliest retained change is newer than
            // `last_seen + 1`, we've lost entries the caller would have
            // needed. Bail and let them rebuild.
            if *earliest > last_seen + 1 {
                return None;
            }
        }
        Some(self.changes.iter().filter(move |(g, _)| *g > last_seen))
    }

    /// Generation of the oldest retained change, or the current
    /// generation when the history is empty. Useful for consumers
    /// deciding whether their last-seen value is still serviceable.
    pub fn earliest_retained_generation(&self) -> u64 {
        self.changes
            .front()
            .map(|(g, _)| *g)
            .unwrap_or(self.generation)
    }

    /// Push a change onto the ring buffer, dropping the oldest entry
    /// when the capacity is exceeded.
    fn push_change(&mut self, change: ModelicaChange) {
        if self.changes.len() >= CHANGE_HISTORY_CAPACITY {
            self.changes.pop_front();
        }
        self.changes.push_back((self.generation, change));
    }

    /// Byte length of the source.
    pub fn len(&self) -> usize {
        self.source.len()
    }

    /// True when the source buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.source.is_empty()
    }

    /// Where this document came from — drives Save behaviour, tab
    /// title, read-only badges. See [`DocumentOrigin`].
    pub fn origin(&self) -> &DocumentOrigin {
        &self.origin
    }

    /// File path the source was loaded from, if any. `None` for
    /// untitled in-memory documents (Save-As required before
    /// write-back). Convenience wrapper over
    /// [`DocumentOrigin::canonical_path`].
    pub fn canonical_path(&self) -> Option<&Path> {
        self.origin.canonical_path()
    }

    /// True when this document is treated as read-only by the UI —
    /// i.e. it does not accept mutating ops. Library / bundled
    /// origins are read-only; Untitled scratch docs are NOT (they
    /// can't save without Save-As but they accept every edit).
    /// Mirrors [`DocumentOrigin::accepts_mutations`].
    pub fn is_read_only(&self) -> bool {
        !self.origin.accepts_mutations()
    }

    /// Set or change the origin (e.g. after Save-As binds a path to
    /// an untitled document). Does not touch generation or source.
    pub fn set_origin(&mut self, origin: DocumentOrigin) {
        self.origin = origin;
    }

    /// Back-compat setter: change the canonical path while keeping
    /// the current writability classification. For Untitled docs,
    /// binding a path promotes them to a writable `File` origin.
    pub fn set_canonical_path(&mut self, path: Option<PathBuf>) {
        match path {
            Some(p) => {
                let writable = self.origin.is_writable() || self.origin.is_untitled();
                self.origin = DocumentOrigin::File { path: p, writable };
            }
            None => {
                // Reverting to untitled drops the path; name defaults
                // to the current display name.
                self.origin = DocumentOrigin::untitled(self.origin.display_name());
            }
        }
    }

    /// Whether the document has unsaved changes — current generation
    /// differs from the last-saved one, or it has never been saved.
    pub fn is_dirty(&self) -> bool {
        match self.last_saved_generation {
            Some(g) => g != self.generation,
            None => true,
        }
    }

    /// Record that the document was just persisted at its current
    /// generation. The Save observer calls this after a successful
    /// disk write.
    pub fn mark_saved(&mut self) {
        self.last_saved_generation = Some(self.generation);
    }
}

/// Recursive class lookup by short name — checks top-level classes
/// first, then walks nested-class trees. Used by `load_msl_class` to
/// locate the target class inside a wrapper package file.
fn find_class_by_short_name_recursive<'a>(
    ast: &'a rumoca_session::parsing::ast::StoredDefinition,
    short: &str,
) -> Option<&'a rumoca_session::parsing::ast::ClassDef> {
    fn walk<'a>(
        classes: &'a indexmap::IndexMap<String, rumoca_session::parsing::ast::ClassDef>,
        short: &str,
    ) -> Option<&'a rumoca_session::parsing::ast::ClassDef> {
        if let Some(c) = classes.get(short) {
            return Some(c);
        }
        for c in classes.values() {
            if let Some(found) = walk(&c.classes, short) {
                return Some(found);
            }
        }
        None
    }
    walk(&ast.classes, short)
}

/// The op type for [`ModelicaDocument`].
///
/// Text-level ops land today. AST-level ops (`SetParameter`,
/// `AddComponent`, `AddConnection`, `SetPlacement`, …) arrive alongside
/// the pretty-printer in a follow-up commit; they will be expressed as
/// span-based [`EditText`](Self::EditText) patches internally so
/// surrounding formatting and comments stay intact.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ModelicaOp {
    /// Replace the entire source buffer. The inverse is another
    /// `ReplaceSource` carrying the previous source text.
    ReplaceSource {
        /// The new source text to install.
        new: String,
    },
    /// Replace a byte range with new text. Used by granular text
    /// editors and by AST-level ops that splice at a span.
    ///
    /// The inverse is another `EditText` whose `range` covers the
    /// newly-inserted text and whose `replacement` is the text that
    /// was removed.
    EditText {
        /// Byte range in the current source buffer to replace.
        /// Must fall on `char` boundaries.
        range: Range<usize>,
        /// Replacement text. May be shorter or longer than `range.len()`.
        replacement: String,
    },
    /// Append a new component declaration to the body of `class`.
    ///
    /// Insertion point is chosen AST-aware: just before the
    /// `equation`/`algorithm` keyword if the class has one, otherwise
    /// just before `end ClassName;`. The decl is rendered via
    /// [`crate::pretty::component_decl`] and spliced as an [`EditText`]
    /// internally, so the inverse is a straightforward deletion
    /// (`EditText` with empty replacement).
    AddComponent {
        /// Target class name. Either a single-segment top-level name
        /// (`"Circuit"`) or a dotted qualified path for nested classes
        /// (`"Pkg.Inner"`, `"A.B.C"`). Case-sensitive.
        class: String,
        /// Declaration payload. See [`pretty::ComponentDecl`].
        decl: ComponentDecl,
    },
    /// Append a new `connect(...)` equation inside `class`'s equation
    /// section. Creates an `equation` section if one does not exist.
    ///
    /// Rendered via [`crate::pretty::connect_equation`] and spliced as
    /// an [`EditText`] internally; inverse is the matching deletion.
    AddConnection {
        /// Target class name. Accepts dotted qualified paths for
        /// nested classes (e.g. `"Pkg.Inner"`).
        class: String,
        /// Equation payload. See [`pretty::ConnectEquation`].
        eq: ConnectEquation,
    },
    /// Remove a component declaration from `class` by instance name.
    ///
    /// Removes the whole declaration line(s) including the trailing
    /// semicolon and newline. Uses `Component.location` as the span
    /// anchor. Inverse is an [`EditText`] that reinserts the deleted
    /// text verbatim — including any comments / annotations that were
    /// attached to the declaration.
    RemoveComponent {
        /// Target class name. Accepts dotted qualified paths for
        /// nested classes (e.g. `"Pkg.Inner"`).
        class: String,
        /// Instance name to remove.
        name: String,
    },
    /// Remove a `connect(from.component.from.port, to.component.to.port)`
    /// equation from `class`. Matches by component+port pair (order
    /// insensitive).
    ///
    /// Spans the full equation including the trailing semicolon and
    /// any trailing `annotation(Line(...))`. Inverse is a byte-exact
    /// [`EditText`] reinsertion.
    RemoveConnection {
        /// Target class name.
        class: String,
        /// One endpoint of the connection.
        from: pretty::PortRef,
        /// Other endpoint.
        to: pretty::PortRef,
    },
    /// Set or replace the `Placement` annotation on a component.
    ///
    /// If the component already has an `annotation(Placement(...))`,
    /// the Placement fragment is replaced in place. Otherwise a fresh
    /// `annotation(Placement(...))` is inserted just before the
    /// declaration's trailing semicolon. Other annotations (Dialog,
    /// Documentation, etc.) are preserved in both cases.
    SetPlacement {
        /// Target class name.
        class: String,
        /// Component instance name.
        name: String,
        /// New placement.
        placement: pretty::Placement,
    },
    /// Set or replace a parameter modification on a component.
    ///
    /// If the component declaration already carries a modifications
    /// list with `param = …`, the right-hand side is replaced. If the
    /// list exists but the param is absent, `param = value` is appended.
    /// If no modifications list exists yet, a `(param = value)` is
    /// inserted after the component name.
    SetParameter {
        /// Target class name.
        class: String,
        /// Component instance name.
        component: String,
        /// Parameter / modifier name.
        param: String,
        /// Replacement value expression (emitted verbatim).
        value: String,
    },
    /// Append a `__LunCo_PlotNode(...)` vendor entry to the class's
    /// `annotation(Diagram(graphics={...}))` array. Creates the
    /// `Diagram(...)` annotation (and its `graphics={}` array) if
    /// they don't exist yet.
    ///
    /// Identification key: `signal_path`. The combination of class +
    /// signal_path is treated as unique — adding a plot for a
    /// signal that already has one in source replaces the existing
    /// entry's extent / title (mirrors how `SetPlacement` handles a
    /// pre-existing `Placement(...)`). Multiple plots of the same
    /// signal aren't supported by this op; use `EditText` for that.
    AddPlotNode {
        /// Target class name.
        class: String,
        /// Plot annotation to add or update.
        plot: pretty::LunCoPlotNodeSpec,
    },
    /// Remove the first `__LunCo_PlotNode(...)` vendor entry whose
    /// `signal=` matches `signal_path` from the class's
    /// `Diagram(graphics)` array. No-op error when no matching entry
    /// exists — the canvas-side delete should hide the row anyway.
    RemovePlotNode {
        /// Target class name.
        class: String,
        /// Signal path identifying which plot entry to remove.
        signal_path: String,
    },
    /// Update the `extent={{...}}` argument of the first
    /// `__LunCo_PlotNode(...)` entry whose `signal=` matches
    /// `signal_path`. Emitted by canvas drag/resize gestures so the
    /// stored layout follows the user's manipulation.
    SetPlotNodeExtent {
        /// Target class name.
        class: String,
        /// Signal path identifying which plot entry to update.
        signal_path: String,
        /// New rectangle in diagram coordinates.
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
    /// Update the `title=` argument (or insert one) on the first
    /// `__LunCo_PlotNode(...)` entry whose `signal=` matches
    /// `signal_path`. Empty `title` removes the field entirely.
    SetPlotNodeTitle {
        /// Target class name.
        class: String,
        /// Signal path identifying which plot entry to update.
        signal_path: String,
        /// New title (empty → remove the `title=` field).
        title: String,
    },
    /// Replace the `extent={{…}}` argument of the i-th `Text(...)`
    /// entry inside the class's `Diagram(graphics)` array. Used by
    /// canvas drag/resize on editable diagram labels.
    SetDiagramTextExtent {
        /// Target class name.
        class: String,
        /// Index of the Text item within `Diagram(graphics={...})`.
        /// Counted in source order, Text-only.
        index: usize,
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
    /// Replace the `textString=` argument of the i-th `Text(...)`
    /// entry inside the class's `Diagram(graphics)` array. Empty
    /// `text` is allowed (Modelica permits empty Text labels).
    SetDiagramTextString {
        /// Target class name.
        class: String,
        /// Index of the Text item within `Diagram(graphics={...})`.
        index: usize,
        /// New `textString=` value (the writer adds the quotes).
        text: String,
    },
    /// Remove the i-th `Text(...)` entry from the class's
    /// `Diagram(graphics)` array.
    RemoveDiagramText {
        /// Target class name.
        class: String,
        /// Index of the Text item within `Diagram(graphics={...})`.
        index: usize,
    },

    // ── Layer 2: full class authoring ───────────────────────────────────────
    /// Add a new empty class (model/connector/package/...) inside `parent`
    /// (or at top level when `parent` is empty).
    AddClass {
        parent: String,
        name: String,
        kind: pretty::ClassKindSpec,
        description: String,
        partial: bool,
    },
    /// Remove a class by qualified name (and its trailing whitespace).
    RemoveClass { qualified: String },
    /// Add a short-class definition: `connector X = Y(...)`,
    /// `connector Out = output Real(unit="N")`, etc.
    AddShortClass {
        parent: String,
        name: String,
        kind: pretty::ClassKindSpec,
        base: String,
        prefixes: Vec<String>,
        modifications: Vec<(String, String)>,
    },
    /// Add a variable declaration to a class body.
    AddVariable {
        class: String,
        decl: pretty::VariableDecl,
    },
    /// Remove a variable declaration by name.
    RemoveVariable { class: String, name: String },
    /// Append an equation to a class equation section, creating one if needed.
    AddEquation {
        class: String,
        eq: pretty::EquationDecl,
    },
    /// Append a graphic to the class's `annotation(Icon(graphics={...}))`,
    /// creating the wrapper if needed.
    AddIconGraphic {
        class: String,
        graphic: pretty::GraphicSpec,
    },
    /// Append a graphic to the class's `annotation(Diagram(graphics={...}))`,
    /// creating the wrapper if needed.
    AddDiagramGraphic {
        class: String,
        graphic: pretty::GraphicSpec,
    },
    /// Set or replace the `experiment(...)` argument inside the class
    /// annotation.
    SetExperimentAnnotation {
        class: String,
        start_time: f64,
        stop_time: f64,
        tolerance: f64,
        interval: f64,
    },
}

impl DocumentOp for ModelicaOp {}

impl Document for ModelicaDocument {
    type Op = ModelicaOp;

    fn id(&self) -> DocumentId {
        self.id
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn apply(&mut self, op: Self::Op) -> Result<Self::Op, DocumentError> {
        // Read-only enforcement — the document is the single
        // source of truth for its own mutability. Origin says
        // whether this doc accepts ops; if not, every caller
        // (palette, inspector, canvas drag-drop, API) gets the
        // same `ReadOnly` error and surfaces it through their
        // normal error paths (no band-aid pre-checks needed in
        // panels). Cosmetic ops are not exempt — a frozen MSL
        // class shouldn't accept SetPlacement either; users
        // duplicate-to-workspace if they want to lay it out.
        //
        // **`accepts_mutations()`, not `is_writable()`** —
        // `is_writable` returns `false` for Untitled docs (Save-As
        // required), but Untitled docs are the canonical scratch
        // surface and must accept all edits. Conflating the two
        // silently bricks the duplicate-to-workspace flow.
        if !self.origin.accepts_mutations() {
            return Err(DocumentError::ReadOnly);
        }
        // Translate any op to a (range, replacement, change) triple.
        // `ReplaceSource` is expressed as replacing the full buffer —
        // no special-casing needed below. Every op follows the same
        // mutate / bump-generation / refresh-cache / emit-change path.
        let (range, replacement, change) =
            op_to_patch(&self.source, &self.syntax, &self.syntax.ast, op)?;
        self.apply_patch(range, replacement, change)
    }
}

impl ModelicaDocument {
    /// Core mutation path. All ops funnel through here so the document
    /// has a single source-of-truth for generation bumps, AST refresh,
    /// and change emission.
    ///
    /// Returns an [`ModelicaOp::EditText`] inverse carrying the exact
    /// removed bytes — uniform undo for every op kind.
    fn apply_patch(
        &mut self,
        range: Range<usize>,
        replacement: String,
        change: ModelicaChange,
    ) -> Result<ModelicaOp, DocumentError> {
        if range.start > range.end || range.end > self.source.len() {
            return Err(DocumentError::ValidationFailed(format!(
                "text range {}..{} out of bounds (len={})",
                range.start,
                range.end,
                self.source.len()
            )));
        }
        if !self.source.is_char_boundary(range.start)
            || !self.source.is_char_boundary(range.end)
        {
            return Err(DocumentError::ValidationFailed(format!(
                "text range {}..{} not on char boundaries",
                range.start, range.end
            )));
        }
        let removed: String = self.source[range.clone()].to_string();
        self.source.replace_range(range.clone(), &replacement);
        self.generation = self.generation.saturating_add(1);
        // AST reparse is DEFERRED: rumoca's `parse_to_ast` is fast
        // (~ms) on small files but adds up under rapid typing,
        // especially while the sim worker is pushing sample batches
        // through the main thread's signal-registry drain. The old
        // eager reparse here turned every keystroke during a run
        // into a visibly laggy frame. We now:
        //   * bump `generation` immediately so save/undo paths see
        //     the mutation right away,
        //   * stamp `last_source_edit_at` so the debouncing
        //     reparse system in `ui/ast_refresh.rs` knows when the
        //     quiet window starts,
        //   * keep the previous `AstCache` as a stale-but-usable
        //     snapshot. Consumers that strictly need a fresh parse
        //     (Compile) check `ast().generation == self.generation`
        //     and force a reparse via `refresh_ast_now()`.
        self.last_source_edit_at = Some(web_time::Instant::now());
        // Optimistic Index patch BEFORE push_change so panel-side
        // observers reading the Index see the new entries by the time
        // they get the change notification. Reconcile happens on the
        // next AST refresh; structural ops here never disagree with
        // the eventual reparse result.
        match &change {
            ModelicaChange::ComponentAdded { class, name } => {
                // The op-to-patch step doesn't carry the type name into
                // the change event today (it's encoded in the spliced
                // text). Best-effort: stash an empty placeholder; the
                // reconcile fills it in. Panels that need the type
                // immediately read the change event payload.
                self.index.patch_component_added(class, name, "");
            }
            ModelicaChange::ComponentRemoved { class, name } => {
                self.index.patch_component_removed(class, name);
            }
            ModelicaChange::PlacementChanged {
                class,
                component,
                placement,
            } => {
                self.index
                    .patch_placement_changed(class, component, *placement);
            }
            ModelicaChange::ConnectionAdded { class, from, to } => {
                let from_port = if from.port.is_empty() { None } else { Some(from.port.as_str()) };
                let to_port = if to.port.is_empty() { None } else { Some(to.port.as_str()) };
                self.index.patch_connection_added(
                    class,
                    &from.component,
                    from_port,
                    &to.component,
                    to_port,
                );
            }
            ModelicaChange::ConnectionRemoved { class, from, to } => {
                let from_port = if from.port.is_empty() { None } else { Some(from.port.as_str()) };
                let to_port = if to.port.is_empty() { None } else { Some(to.port.as_str()) };
                self.index.patch_connection_removed(
                    class,
                    &from.component,
                    from_port,
                    &to.component,
                    to_port,
                );
            }
            ModelicaChange::ParameterChanged {
                class,
                component,
                param,
                value,
            } => {
                self.index.patch_parameter_changed(class, component, param, value);
            }
            ModelicaChange::ClassAdded { qualified, kind } => {
                self.index
                    .patch_class_added(qualified, class_kind_spec_to_index_kind(*kind));
            }
            ModelicaChange::ClassRemoved { qualified } => {
                self.index.patch_class_removed(qualified);
            }
            // TextReplaced — opaque text edit, Index reconciles on
            // the next reparse since we don't know what changed
            // structurally.
            _ => {}
        }
        self.push_change(change);
        let inverse_range = range.start..(range.start + replacement.len());
        Ok(ModelicaOp::EditText {
            range: inverse_range,
            replacement: removed,
        })
    }
}

/// Map the op-layer's [`pretty::ClassKindSpec`] to the Index's
/// [`crate::index::ClassKind`]. The op layer doesn't author the
/// extended kinds (ExpandableConnector, Operator, OperatorRecord,
/// Class) — those only appear via parse, never via API ops.
fn class_kind_spec_to_index_kind(spec: pretty::ClassKindSpec) -> crate::index::ClassKind {
    use crate::index::ClassKind;
    match spec {
        pretty::ClassKindSpec::Model => ClassKind::Model,
        pretty::ClassKindSpec::Block => ClassKind::Block,
        pretty::ClassKindSpec::Connector => ClassKind::Connector,
        pretty::ClassKindSpec::Package => ClassKind::Package,
        pretty::ClassKindSpec::Record => ClassKind::Record,
        pretty::ClassKindSpec::Function => ClassKind::Function,
        pretty::ClassKindSpec::Type => ClassKind::Type,
    }
}

/// Translate a high-level [`ModelicaOp`] into the concrete text patch
/// and the structured change it represents. Pure function — no
/// document state mutated.
/// Bail with the same parse-error message `resolve_class` produces, so
/// AST-canonical ops in `op_to_patch` surface parse failures
/// identically to the legacy `pretty/`-based helpers (which call
/// `resolve_class` internally). Without this guard, `regenerate_class_patch`
/// would happily run against a stale-but-parseable AST and produce a
/// patch that erases the user's in-flight typing on the next reparse.
fn ast_check_no_parse_error(ast: &AstCache) -> Result<(), DocumentError> {
    if let Some(msg) = ast.first_error() {
        return Err(DocumentError::ValidationFailed(format!(
            "cannot apply AST op while source has a parse error: {}",
            msg
        )));
    }
    Ok(())
}

/// Translate a structural-mutation error into the `DocumentError`
/// shape `op_to_patch` callers expect. `ValidationFailed` is the
/// catch-all the legacy path uses for "couldn't find the target" /
/// "couldn't construct the new value", so we reuse it here for parity.
fn ast_mut_to_doc_error(e: crate::ast_mut::AstMutError) -> DocumentError {
    DocumentError::ValidationFailed(e.to_string())
}

fn op_to_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    op: ModelicaOp,
) -> Result<(Range<usize>, String, ModelicaChange), DocumentError> {
    match op {
        ModelicaOp::ReplaceSource { new } => Ok((
            0..source.len(),
            new,
            ModelicaChange::TextReplaced,
        )),
        ModelicaOp::EditText { range, replacement } => {
            Ok((range, replacement, ModelicaChange::TextReplaced))
        }
        ModelicaOp::AddComponent { class, decl } => {
            // AST-canonical path (A.2 batch 2). Legacy
            // `compute_add_component_patch` retained below for revert
            // until parity is confirmed. Same `(class_name)` capture
            // pattern as batch 1 — `decl.name` consumed into the
            // `change` payload so we clone here.
            ast_check_no_parse_error(ast)?;
            let added_name = decl.name.clone();
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_component(c, &decl),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ComponentAdded {
                class,
                name: added_name,
            };
            Ok((r, rp, change))
        }
        ModelicaOp::AddConnection { class, eq } => {
            ast_check_no_parse_error(ast)?;
            let from = eq.from.clone();
            let to = eq.to.clone();
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_connection(c, &eq),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ConnectionAdded { class, from, to };
            Ok((r, rp, change))
        }
        ModelicaOp::RemoveComponent { class, name } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::remove_component(c, &name),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ComponentRemoved { class, name };
            Ok((r, rp, change))
        }
        ModelicaOp::RemoveConnection { class, from, to } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::remove_connection(c, &from, &to),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ConnectionRemoved { class, from, to };
            Ok((r, rp, change))
        }
        ModelicaOp::SetPlacement { class, name, placement } => {
            // AST-canonical path (A.2 batch 1). Regenerates the whole
            // class via `to_modelica()` after a structural mutation;
            // legacy `compute_set_placement_patch` retained below for
            // emergency revert until parity is confirmed in the wild.
            // Surface AST-resolution errors as ValidationFailed —
            // matches the legacy path's error type so consumers don't
            // need to handle a new variant.
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_placement(c, &name, &placement),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::PlacementChanged {
                class,
                component: name,
                placement,
            };
            Ok((r, rp, change))
        }
        ModelicaOp::SetParameter { class, component, param, value } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_parameter(c, &component, &param, &value),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ParameterChanged {
                class,
                component,
                param,
                value,
            };
            Ok((r, rp, change))
        }
        ModelicaOp::AddPlotNode { class, plot } => {
            // AST-canonical (A.2 batch 3b — graphics ops). Plot edits
            // only touch the Diagram annotation tree; consumers
            // observe `TextReplaced` and rebuild from source.
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_plot_node(c, &plot),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::RemovePlotNode { class, signal_path } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::remove_plot_node(c, &signal_path),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetPlotNodeExtent { class, signal_path, x1, y1, x2, y2 } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_plot_node_extent(c, &signal_path, x1, y1, x2, y2),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetPlotNodeTitle { class, signal_path, title } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_plot_node_title(c, &signal_path, &title),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetDiagramTextExtent { class, index, x1, y1, x2, y2 } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_diagram_text_extent(c, index, x1, y1, x2, y2),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetDiagramTextString { class, index, text } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_diagram_text_string(c, index, &text),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::RemoveDiagramText { class, index } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::remove_diagram_text(c, index),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::AddClass { parent, name, kind, description, partial } => {
            // AST-canonical (A.2 batch 3). AddClass / RemoveClass
            // change the document's class set, not a single class
            // span — use the whole-document patch helper. The
            // formatter is idempotent (verified by `ast_roundtrip`),
            // so unchanged classes round-trip byte-stably; only the
            // newly-added or removed class block actually shifts.
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_document_patch(source, parsed, |sd| {
                crate::ast_mut::add_class(sd, &parent, &name, kind, &description, partial)
            })
            .map_err(ast_mut_to_doc_error)?;
            let qualified = if parent.is_empty() {
                name.clone()
            } else {
                format!("{}.{}", parent, name)
            };
            Ok((r, rp, ModelicaChange::ClassAdded { qualified, kind }))
        }
        ModelicaOp::RemoveClass { qualified } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_document_patch(source, parsed, |sd| {
                crate::ast_mut::remove_class(sd, &qualified)
            })
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::ClassRemoved { qualified }))
        }
        ModelicaOp::AddShortClass { parent, name, kind, base, prefixes, modifications } => {
            // Same whole-document path as AddClass — both ops change
            // the document's class set. AST-canonical (A.2 batch 3b).
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_document_patch(source, parsed, |sd| {
                crate::ast_mut::add_short_class(
                    sd, &parent, &name, kind, &base, &prefixes, &modifications,
                )
            })
            .map_err(ast_mut_to_doc_error)?;
            let qualified = if parent.is_empty() {
                name.clone()
            } else {
                format!("{}.{}", parent, name)
            };
            Ok((r, rp, ModelicaChange::ClassAdded { qualified, kind }))
        }
        ModelicaOp::AddVariable { class, decl } => {
            // Variables and typed components share `components: IndexMap`
            // in the AST. Same regenerate-class path as AddComponent.
            ast_check_no_parse_error(ast)?;
            let added_name = decl.name.clone();
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_variable(c, &decl),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ComponentAdded {
                class,
                name: added_name,
            };
            Ok((r, rp, change))
        }
        ModelicaOp::RemoveVariable { class, name } => {
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::remove_variable(c, &name),
            )
            .map_err(ast_mut_to_doc_error)?;
            let change = ModelicaChange::ComponentRemoved { class, name };
            Ok((r, rp, change))
        }
        ModelicaOp::AddEquation { class, eq } => {
            // Generic equation append. AST-canonical (A.2 batch 3b).
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_equation(c, &eq),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::AddIconGraphic { class, graphic } => {
            ast_check_no_parse_error(ast)?;
            let graphic_text = crate::pretty::graphic_inner(&graphic);
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_named_graphic(c, "Icon", &graphic_text),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::AddDiagramGraphic { class, graphic } => {
            ast_check_no_parse_error(ast)?;
            let graphic_text = crate::pretty::graphic_inner(&graphic);
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::add_named_graphic(c, "Diagram", &graphic_text),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetExperimentAnnotation { class, start_time, stop_time, tolerance, interval } => {
            // AST-canonical (A.2 batch 3b). Class-level `experiment(...)`
            // is one flat entry in `ClassDef.annotation` — no nested
            // graphics-array navigation required.
            ast_check_no_parse_error(ast)?;
            let (r, rp) = crate::ast_mut::regenerate_class_patch(
                source,
                parsed,
                &class,
                |c| crate::ast_mut::set_experiment(c, start_time, stop_time, tolerance, interval),
            )
            .map_err(ast_mut_to_doc_error)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
    }
}

// ---------------------------------------------------------------------------
// (deleted in A.4) Legacy `compute_*_patch` text-splice helpers used to live
// here — ~1800 lines that turned each AST-shaped op into a `(byte_range,
// replacement)` patch by hand-walking the source via `find_annotation_span`,
// `find_placement_span`, `find_named_call_span`, `find_statement_terminator`,
// and `modify_mod_list`. After A.2 every op routes through
// `crate::ast_mut::regenerate_class_patch` / `regenerate_document_patch`,
// so the splice helpers are unreferenced. Removed for two reasons:
//
// 1. Eliminates the UTF-8 byte-boundary risk class — the helpers
//    indexed `&str` by raw byte offsets and panicked when a
//    multi-byte char straddled an insertion point.
// 2. Eliminates a parallel mutation surface the chokepoint discipline
//    in AGENTS.md §4.1 had to keep policed.
//
// The companion class-duplication path (`rewrite_inject_in_one_pass`
// in `ui/commands.rs`) is independent — it duplicates a class via
// byte-splice into a new file, never touching the AST-mutation
// chokepoint — and stays for now.

#[cfg(test)]
mod tests {
    use super::*;
    use lunco_doc::DocumentHost;

    fn doc() -> DocumentHost<ModelicaDocument> {
        DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "model Empty end Empty;\n",
        ))
    }

    #[test]
    fn fresh_document_state() {
        let host = doc();
        assert_eq!(host.generation(), 0);
        assert_eq!(host.document().source(), "model Empty end Empty;\n");
        assert_eq!(host.document().id(), DocumentId::new(1));
        assert!(!host.can_undo());
        assert!(!host.can_redo());
        assert!(!host.document().is_empty());
    }

    #[test]
    fn replace_source_mutates_and_bumps_generation() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "model NewModel end NewModel;".into(),
        })
        .unwrap();
        assert_eq!(host.document().source(), "model NewModel end NewModel;");
        assert_eq!(host.generation(), 1);
        assert!(host.can_undo());
    }

    #[test]
    fn undo_restores_previous_source() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "replaced".into(),
        })
        .unwrap();
        host.undo().unwrap();
        assert_eq!(host.document().source(), "model Empty end Empty;\n");
    }

    #[test]
    fn redo_reapplies_replaced_source() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "replaced".into(),
        })
        .unwrap();
        host.undo().unwrap();
        host.redo().unwrap();
        assert_eq!(host.document().source(), "replaced");
    }

    #[test]
    fn multi_step_undo_redo_round_trip() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource { new: "a".into() }).unwrap();
        host.apply(ModelicaOp::ReplaceSource { new: "b".into() }).unwrap();
        host.apply(ModelicaOp::ReplaceSource { new: "c".into() }).unwrap();
        assert_eq!(host.document().source(), "c");
        assert_eq!(host.generation(), 3);

        host.undo().unwrap();
        host.undo().unwrap();
        host.undo().unwrap();
        assert_eq!(host.document().source(), "model Empty end Empty;\n");

        host.redo().unwrap();
        host.redo().unwrap();
        host.redo().unwrap();
        assert_eq!(host.document().source(), "c");
    }

    #[test]
    fn generation_monotonic_across_undo_redo() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource { new: "a".into() }).unwrap();
        assert_eq!(host.generation(), 1);
        host.undo().unwrap();
        // Undo is itself a mutation — panels that key on generation need a
        // fresh signal either way.
        assert_eq!(host.generation(), 2);
        host.redo().unwrap();
        assert_eq!(host.generation(), 3);
    }

    #[test]
    fn new_apply_clears_redo_branch() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource { new: "first".into() }).unwrap();
        host.undo().unwrap();
        assert!(host.can_redo());

        host.apply(ModelicaOp::ReplaceSource { new: "second".into() }).unwrap();
        assert!(!host.can_redo());
        assert_eq!(host.document().source(), "second");
    }

    #[test]
    fn ast_cache_parses_fresh_document() {
        let host = doc();
        let cache = host.document().ast();
        assert_eq!(cache.generation, 0);
        assert!(cache.result.is_ok(), "fresh doc should parse");
        let ast = host.document().strict_ast().expect("strict_ast Some");
        assert!(ast.classes.contains_key("Empty"));
    }

    #[test]
    fn ast_cache_refreshes_after_mutation() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "model Foo end Foo;".into(),
        })
        .unwrap();
        // AST reparse is debounced — force it so the test sees the
        // new parse deterministically.
        host.document_mut().refresh_ast_now();
        let cache = host.document().ast();
        assert_eq!(cache.generation, 1);
        assert!(cache.result.is_ok(), "strict parse should succeed");
        let ast = host.document().strict_ast().expect("strict_ast Some");
        assert!(ast.classes.contains_key("Foo"));
        assert!(!ast.classes.contains_key("Empty"));
    }

    #[test]
    fn ast_cache_holds_error_on_invalid_source() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "model M Real x end M;".into(), // missing semicolon → parse err
        })
        .unwrap();
        host.document_mut().refresh_ast_now();
        assert!(host.document().ast().has_errors());
    }

    #[test]
    fn ast_stays_stale_until_refresh() {
        // Regression test for the debounce behaviour. Before the
        // debounce change, every keystroke reparsed synchronously,
        // which was a perf hog under rapid typing. Now `apply_patch`
        // leaves the AST stale; `ast_is_stale()` tells callers to
        // wait for the debounce driver or call `refresh_ast_now()`.
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "model Foo end Foo;".into(),
        })
        .unwrap();
        assert!(
            host.document().ast_is_stale(),
            "AST should be stale right after apply_patch"
        );
        // Old cache still usable — matches pre-edit source.
        assert_eq!(host.document().ast().generation, 0);
        host.document_mut().refresh_ast_now();
        assert!(!host.document().ast_is_stale());
        assert_eq!(host.document().ast().generation, 1);
    }

    #[test]
    fn edit_text_replaces_range_and_is_invertible() {
        // "model Empty end Empty;\n"
        //  0         1
        //  0123456789012345678901
        let mut host = doc();
        // Replace "Empty" at positions 6..11 with "Thing"
        host.apply(ModelicaOp::EditText {
            range: 6..11,
            replacement: "Thing".into(),
        })
        .unwrap();
        assert_eq!(host.document().source(), "model Thing end Empty;\n");
        assert_eq!(host.generation(), 1);

        host.undo().unwrap();
        assert_eq!(host.document().source(), "model Empty end Empty;\n");
    }

    #[test]
    fn edit_text_supports_insertion_and_deletion() {
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "abcdef".to_string(),
        ));
        // Insert "XYZ" at position 3 (empty range)
        host.apply(ModelicaOp::EditText {
            range: 3..3,
            replacement: "XYZ".into(),
        })
        .unwrap();
        assert_eq!(host.document().source(), "abcXYZdef");

        // Delete "XYZ" (range 3..6, empty replacement)
        host.apply(ModelicaOp::EditText {
            range: 3..6,
            replacement: String::new(),
        })
        .unwrap();
        assert_eq!(host.document().source(), "abcdef");

        host.undo().unwrap();
        assert_eq!(host.document().source(), "abcXYZdef");
        host.undo().unwrap();
        assert_eq!(host.document().source(), "abcdef");
    }

    #[test]
    fn edit_text_rejects_out_of_bounds_range() {
        let mut host = doc();
        let err = host
            .apply(ModelicaOp::EditText {
                range: 0..999,
                replacement: String::new(),
            })
            .unwrap_err();
        assert!(matches!(err, lunco_doc::Reject::InvalidOp(_)));
        // Unchanged on error.
        assert_eq!(host.document().source(), "model Empty end Empty;\n");
        assert_eq!(host.generation(), 0);
    }

    // ------------------------------------------------------------------
    // AST-level ops: AddComponent / AddConnection
    // ------------------------------------------------------------------

    #[test]
    fn add_component_appends_before_end_when_no_equation_section() {
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "model M\n  Real a;\nend M;\n".to_string(),
        ));
        host.apply(ModelicaOp::AddComponent {
            class: "M".into(),
            decl: ComponentDecl {
                type_name: "Real".into(),
                name: "b".into(),
                modifications: vec![],
                placement: None,
            },
        })
        .unwrap();
        assert_eq!(
            host.document().source(),
            "model M\n  Real a;\n  Real b;\nend M;\n"
        );
        // AST cache is debounced — force a reparse before inspecting.
        host.document_mut().refresh_ast_now();
        let ast = host.document().strict_ast().expect("parse ok");
        assert!(ast.classes.get("M").unwrap().components.contains_key("b"));
    }

    #[test]
    fn add_component_inserts_before_equation_section() {
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "model M\n  Real a;\nequation\n  a = 1;\nend M;\n".to_string(),
        ));
        host.apply(ModelicaOp::AddComponent {
            class: "M".into(),
            decl: ComponentDecl {
                type_name: "Real".into(),
                name: "b".into(),
                modifications: vec![],
                placement: None,
            },
        })
        .unwrap();
        assert_eq!(
            host.document().source(),
            "model M\n  Real a;\n  Real b;\nequation\n  a = 1;\nend M;\n"
        );
    }

    #[test]
    fn add_component_is_invertible() {
        let original = "model M\n  Real a;\nend M;\n";
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            original.to_string(),
        ));
        host.apply(ModelicaOp::AddComponent {
            class: "M".into(),
            decl: ComponentDecl {
                type_name: "Real".into(),
                name: "b".into(),
                modifications: vec![],
                placement: None,
            },
        })
        .unwrap();
        host.undo().unwrap();
        assert_eq!(host.document().source(), original);
    }

    #[test]
    fn add_component_errors_on_unknown_class() {
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "model M end M;\n".to_string(),
        ));
        let err = host
            .apply(ModelicaOp::AddComponent {
                class: "Other".into(),
                decl: ComponentDecl {
                    type_name: "Real".into(),
                    name: "x".into(),
                    modifications: vec![],
                    placement: None,
                },
            })
            .unwrap_err();
        assert!(matches!(err, lunco_doc::Reject::InvalidOp(_)));
        assert_eq!(host.generation(), 0);
    }

    #[test]
    fn add_connection_appends_to_existing_equation_section() {
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "model M\n  Real a;\n  Real b;\nequation\n  a = 1;\nend M;\n".to_string(),
        ));
        host.apply(ModelicaOp::AddConnection {
            class: "M".into(),
            eq: ConnectEquation {
                from: crate::pretty::PortRef::new("a", "p"),
                to: crate::pretty::PortRef::new("b", "n"),
                line: None,
            },
        })
        .unwrap();
        assert_eq!(
            host.document().source(),
            "model M\n  Real a;\n  Real b;\nequation\n  a = 1;\n  connect(a.p, b.n);\nend M;\n"
        );
    }

    #[test]
    fn add_connection_creates_equation_section_when_missing() {
        let mut host = DocumentHost::new(ModelicaDocument::new(
            DocumentId::new(1),
            "model M\n  Real a;\n  Real b;\nend M;\n".to_string(),
        ));
        host.apply(ModelicaOp::AddConnection {
            class: "M".into(),
            eq: ConnectEquation {
                from: crate::pretty::PortRef::new("a", "p"),
                to: crate::pretty::PortRef::new("b", "n"),
                line: None,
            },
        })
        .unwrap();
        assert_eq!(
            host.document().source(),
            "model M\n  Real a;\n  Real b;\nequation\n  connect(a.p, b.n);\nend M;\n"
        );
    }

    #[test]
    fn add_component_rejects_when_source_has_parse_error() {
        let mut host = doc();
        host.apply(ModelicaOp::ReplaceSource {
            new: "model M Real x end M;".into(),
        })
        .unwrap();
        let err = host
            .apply(ModelicaOp::AddComponent {
                class: "M".into(),
                decl: ComponentDecl {
                    type_name: "Real".into(),
                    name: "y".into(),
                    modifications: vec![],
                    placement: None,
                },
            })
            .unwrap_err();
        assert!(matches!(err, lunco_doc::Reject::InvalidOp(_)));
    }
}
