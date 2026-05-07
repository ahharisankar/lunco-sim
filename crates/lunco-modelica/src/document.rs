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
            let (r, rp) = compute_add_plot_node_patch(source, ast, parsed, &class, &plot)?;
            // Plot edits don't move components or rewire connections;
            // they only affect the diagram-decoration layer. The
            // projection still re-reads the diagram annotation on
            // every change, so `TextReplaced` is the cleanest signal:
            // consumers that care rebuild from source.
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::RemovePlotNode { class, signal_path } => {
            let (r, rp) = compute_remove_plot_node_patch(source, ast, parsed, &class, &signal_path)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetPlotNodeExtent { class, signal_path, x1, y1, x2, y2 } => {
            let (r, rp) = compute_set_plot_node_extent_patch(
                source, ast, parsed, &class, &signal_path, x1, y1, x2, y2,
            )?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetPlotNodeTitle { class, signal_path, title } => {
            let (r, rp) = compute_set_plot_node_title_patch(
                source, ast, parsed, &class, &signal_path, &title,
            )?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetDiagramTextExtent { class, index, x1, y1, x2, y2 } => {
            let (r, rp) = compute_set_diagram_text_extent_patch(
                source, ast, parsed, &class, index, x1, y1, x2, y2,
            )?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::SetDiagramTextString { class, index, text } => {
            let (r, rp) = compute_set_diagram_text_string_patch(
                source, ast, parsed, &class, index, &text,
            )?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::RemoveDiagramText { class, index } => {
            let (r, rp) = compute_remove_diagram_text_patch(source, ast, parsed, &class, index)?;
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
            let (r, rp) = compute_add_named_graphic_patch(source, ast, parsed, &class, "Icon", &graphic)?;
            Ok((r, rp, ModelicaChange::TextReplaced))
        }
        ModelicaOp::AddDiagramGraphic { class, graphic } => {
            let (r, rp) = compute_add_named_graphic_patch(source, ast, parsed, &class, "Diagram", &graphic)?;
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
// AST-level op helpers
// ---------------------------------------------------------------------------
//
// These functions turn a high-level AST-level op request into a concrete
// `(range, replacement)` text patch, using the cached AST's token spans
// to locate insertion points. They never mutate the document directly —
// apply() delegates to `EditText`, which gives us uniform undo behavior
// and keeps all source mutation on one code path.
//
// Class resolution accepts qualified dotted paths (e.g. `Pkg.Inner` or
// `Modelica.Electrical.Analog.Basic.Resistor`) and walks the nested
// `ClassDef.classes` index one segment at a time.

/// Resolve a class by qualified name. Accepts:
///
/// - Single-segment names for top-level classes: `"Circuit"`.
/// - Dotted qualified names that drill into nested classes:
///   `"Pkg.Inner"`, `"Modelica.Electrical.Analog.Basic.Resistor"`.
///
/// Each segment must match a class in the previous segment's
/// `classes` index (top-level for the first segment). Returns a
/// [`DocumentError::ValidationFailed`] with the first missing segment
/// named when resolution fails, and an error when the AST is currently
/// a parse failure.
fn resolve_class<'a>(
    ast: &AstCache,
    parsed: &'a StoredDefinition,
    class: &str,
) -> Result<&'a rumoca_session::parsing::ast::ClassDef, DocumentError> {
    if let Some(msg) = ast.first_error() {
        return Err(DocumentError::ValidationFailed(format!(
            "cannot apply AST op while source has a parse error: {}",
            msg
        )));
    }
    let stored = parsed;
    if class.is_empty() {
        return Err(DocumentError::ValidationFailed(
            "class path is empty".into(),
        ));
    }
    let mut segments = class.split('.');
    let first = segments.next().expect("split always yields at least one item");
    let mut current = stored.classes.get(first).ok_or_else(|| {
        DocumentError::ValidationFailed(format!(
            "class `{}` not found in document",
            first,
        ))
    })?;
    let mut walked = first.to_string();
    for segment in segments {
        walked.push('.');
        walked.push_str(segment);
        current = current.classes.get(segment).ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "class `{}` not found (resolving `{}`)",
                walked, class,
            ))
        })?;
    }
    Ok(current)
}

/// Return the byte offset of the start of the line containing `byte_pos`.
/// Used to splice whole lines instead of mid-line inserts — keeps the
/// resulting source readable and the patch ranges easy to reason about.
fn line_start_byte(source: &str, byte_pos: usize) -> usize {
    source[..byte_pos.min(source.len())]
        .rfind('\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

/// Compute the text patch for `AddComponent`.
///
/// Insertion point (first match wins):
///   1. start of the line containing `equation` / `initial equation` /
///      `algorithm` / `initial algorithm` keyword, whichever appears first;
///   2. start of the line containing the `end ClassName;` clause.
///
/// Returns the patch as `(empty_range_at_insertion_point, rendered_decl)`.
fn compute_add_component_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    decl: &ComponentDecl,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    // Defensive: refuse to inject a component declaration into a
    // `package` class. Modelica forbids package-level component
    // declarations (per spec: a package may only contain classes,
    // constants, and operator overloads), so a naive splice produces
    // a parse error that bricks the file for every subsequent
    // AST-based op. The right-click menu and palette already pass
    // the *inner* class name in the post-spec-035 path, but this
    // belt-and-braces check stops the same crash if a future caller
    // forgets — `op_to_patch` is the last gate before the source
    // mutates.
    if matches!(
        class_def.class_type,
        rumoca_session::parsing::ast::ClassType::Package
    ) {
        return Err(DocumentError::ValidationFailed(format!(
            "cannot add component `{}` directly into package `{}`. \
             Add it into one of the package's classes instead.",
            decl.name, class
        )));
    }
    let insertion_byte = class_section_insertion_point(class_def).ok_or_else(|| {
        DocumentError::ValidationFailed(format!(
            "could not locate insertion point in class `{}`",
            class
        ))
    })?;
    let line_start = line_start_byte(source, insertion_byte);
    Ok((line_start..line_start, pretty::component_decl(decl)))
}

/// Compute the text patch for `AddConnection`.
///
/// If the class has an `equation` section, insert the connect equation at
/// the start of the `end` line (appending to the section). If not, insert
/// `equation\n<connect>\n` at the `end` line so a fresh section is created.
fn compute_add_connection_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    eq: &ConnectEquation,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;

    let end_name_byte = class_def
        .end_name_token
        .as_ref()
        .map(|t| t.location.start as usize)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "class `{}` has no `end` clause location",
                class
            ))
        })?;
    let end_line_start = line_start_byte(source, end_name_byte);

    let connect_line = pretty::connect_equation(eq);
    let replacement = if class_def.equation_keyword.is_some() {
        connect_line
    } else {
        format!("equation\n{}", connect_line)
    };

    Ok((end_line_start..end_line_start, replacement))
}

/// Locate the best byte position to insert a new component declaration
/// into a class — just before the first body-section keyword, or if none
/// exists, just before the class's `end` clause.
fn class_section_insertion_point(
    class_def: &rumoca_session::parsing::ast::ClassDef,
) -> Option<usize> {
    let keyword_positions = [
        class_def.equation_keyword.as_ref(),
        class_def.initial_equation_keyword.as_ref(),
        class_def.algorithm_keyword.as_ref(),
        class_def.initial_algorithm_keyword.as_ref(),
    ];
    let earliest_keyword = keyword_positions
        .into_iter()
        .flatten()
        .map(|t| t.location.start as usize)
        .min();
    if let Some(pos) = earliest_keyword {
        return Some(pos);
    }
    class_def
        .end_name_token
        .as_ref()
        .map(|t| t.location.start as usize)
}

/// Extend a declaration/equation span to swallow leading indentation
/// and a trailing newline, so removal leaves a clean source buffer
/// without a dangling blank line.
fn extend_span_to_whole_lines(source: &str, raw: Range<usize>) -> Range<usize> {
    let line_start = source[..raw.start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    // Extend backward only past whitespace on the same line.
    let preceding = &source[line_start..raw.start];
    let start = if preceding.chars().all(|c| c == ' ' || c == '\t') {
        line_start
    } else {
        raw.start
    };
    // Extend forward to and past the following newline if any.
    let end = source[raw.end..]
        .find('\n')
        .map(|i| raw.end + i + 1)
        .unwrap_or(source.len());
    start..end
}

/// Locate the byte position of the semicolon that ends a declaration /
/// equation whose first token starts at `from_byte`. Respects nested
/// parentheses and braces so a `;` inside `annotation(...)` doesn't
/// fool us.
fn find_statement_terminator(source: &str, from_byte: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth: i32 = 0;
    let mut i = from_byte;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'{' | b'[' => depth += 1,
            b')' | b'}' | b']' => depth -= 1,
            b';' if depth <= 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Compute the text patch for `RemoveComponent`.
fn compute_remove_component_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    name: &str,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let component = class_def.components.get(name).ok_or_else(|| {
        DocumentError::ValidationFailed(format!(
            "component `{}` not found in class `{}`",
            name, class
        ))
    })?;
    let raw_start = component.location.start as usize;
    // Component.location.end sometimes stops before the semicolon
    // depending on rumoca's recording — be conservative and extend
    // via terminator scan.
    let term = find_statement_terminator(source, component.name_token.location.start as usize)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "could not find `;` terminating component `{}`",
                name
            ))
        })?;
    let span = extend_span_to_whole_lines(source, raw_start..(term + 1));
    Ok((span, String::new()))
}

/// Match a `ComponentReference` against a `PortRef` (expected form
/// `component.port`). Returns true when the dotted AST path equals the
/// two-part PortRef pair, in that order.
fn cref_matches_port(
    cref: &rumoca_session::parsing::ast::ComponentReference,
    port: &pretty::PortRef,
) -> bool {
    use rumoca_session::parsing::ast::ComponentRefPart;
    let parts: Vec<&ComponentRefPart> = cref.parts.iter().collect();
    if parts.len() != 2 {
        return false;
    }
    parts[0].ident.text.as_ref() == port.component
        && parts[1].ident.text.as_ref() == port.port
}

/// Compute the text patch for `RemoveConnection`.
fn compute_remove_connection_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    from: &pretty::PortRef,
    to: &pretty::PortRef,
) -> Result<(Range<usize>, String), DocumentError> {
    use rumoca_session::parsing::ast::Equation;
    let class_def = resolve_class(ast, parsed, class)?;
    let eq = class_def
        .equations
        .iter()
        .find(|e| match e {
            Equation::Connect { lhs, rhs, .. } => {
                (cref_matches_port(lhs, from) && cref_matches_port(rhs, to))
                    || (cref_matches_port(lhs, to) && cref_matches_port(rhs, from))
            }
            _ => false,
        })
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "connect({}.{}, {}.{}) not found in class `{}`",
                from.component, from.port, to.component, to.port, class
            ))
        })?;
    let start_loc = eq.get_location().ok_or_else(|| {
        DocumentError::Internal("matched connect equation has no location".into())
    })?;
    let raw_start = start_loc.start as usize;
    // Scan backward to the `connect` keyword if it precedes the first
    // component-ref token (it always does for a well-formed connect
    // equation, but ComponentReference.get_location reports the lhs
    // cref's first token).
    let connect_start = source[..raw_start]
        .rfind("connect")
        .filter(|&i| source[i..].starts_with("connect") && i + 7 <= raw_start)
        .unwrap_or(raw_start);
    let term = find_statement_terminator(source, raw_start).ok_or_else(|| {
        DocumentError::ValidationFailed("could not find `;` terminating connect equation".into())
    })?;
    let span = extend_span_to_whole_lines(source, connect_start..(term + 1));
    Ok((span, String::new()))
}

/// Locate a top-level `annotation(` substring inside `[start, end)`,
/// respecting nesting (i.e. must not be inside another parenthesized
/// expression). Returns the byte range covering the whole
/// `annotation(...)` including the outer parens.
fn find_annotation_span(source: &str, span: Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(span.clone())?;
    // Walk the slice tracking paren depth; look for `annotation(` at
    // depth 0.
    let bytes = slice.as_bytes();
    let mut depth: i32 = 0;
    let mut i: usize = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'(' || c == b'{' || c == b'[' {
            depth += 1;
            i += 1;
            continue;
        }
        if c == b')' || c == b'}' || c == b']' {
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 0 && bytes[i..].starts_with(b"annotation") {
            // Check that the preceding char is not an ident char so we
            // don't match `myannotation(`.
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if prev_ok {
                // Skip the keyword and locate the `(`.
                let mut j = i + "annotation".len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    // Find matching `)`.
                    let mut d = 0;
                    let mut k = j;
                    while k < bytes.len() {
                        match bytes[k] {
                            b'(' | b'{' | b'[' => d += 1,
                            b')' | b'}' | b']' => {
                                d -= 1;
                                if d == 0 {
                                    return Some((span.start + i)..(span.start + k + 1));
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    return None;
                }
            }
        }
        i += 1;
    }
    None
}

/// Find the span of the first `Placement(...)` call inside a byte
/// range, matched at top level (paren depth 0 within the range).
fn find_placement_span(source: &str, span: Range<usize>) -> Option<Range<usize>> {
    let slice = source.get(span.clone())?;
    let bytes = slice.as_bytes();
    let mut i: usize = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"Placement") {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            if prev_ok {
                let mut j = i + "Placement".len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    let mut d = 0;
                    let mut k = j;
                    while k < bytes.len() {
                        match bytes[k] {
                            b'(' | b'{' | b'[' => d += 1,
                            b')' | b'}' | b']' => {
                                d -= 1;
                                if d == 0 {
                                    return Some((span.start + i)..(span.start + k + 1));
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Compute the text patch for `SetPlacement`.
///
/// Strategy:
///   1. If the component's decl has an `annotation(...)` block and that
///      block contains `Placement(...)`, replace the `Placement` call
///      in place — other annotations (Dialog, Documentation) untouched.
///   2. If the decl has an `annotation(...)` block without `Placement`,
///      prepend `Placement(...), ` inside it.
///   3. If there is no annotation at all, insert
///      ` annotation(Placement(...))` just before the decl's `;`.
fn compute_set_placement_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    name: &str,
    placement: &pretty::Placement,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let component = class_def.components.get(name).ok_or_else(|| {
        DocumentError::ValidationFailed(format!(
            "component `{}` not found in class `{}`",
            name, class
        ))
    })?;
    let decl_start = component.location.start as usize;
    let term = find_statement_terminator(source, component.name_token.location.start as usize)
        .ok_or_else(|| {
            DocumentError::ValidationFailed("component decl has no terminating `;`".into())
        })?;
    let decl_span = decl_start..term;
    let new_placement = pretty::placement_inner(placement);

    if let Some(ann_span) = find_annotation_span(source, decl_span.clone()) {
        // ann_span covers `annotation(...)` including outer parens.
        // The interior span is (ann_span.start + "annotation(".len() ..
        // ann_span.end - 1).
        let prefix_len = "annotation(".len();
        let inner_start = ann_span.start + prefix_len;
        let inner_end = ann_span.end - 1;
        if let Some(p_span) = find_placement_span(source, inner_start..inner_end) {
            return Ok((p_span, new_placement));
        } else {
            // Insert Placement fragment at the start of the annotation
            // contents, followed by `, ` to keep the remaining entries
            // well-formed.
            let insert_at = inner_start;
            return Ok((
                insert_at..insert_at,
                format!("{}, ", new_placement),
            ));
        }
    }
    // No annotation at all — insert one just before the `;`.
    Ok((
        term..term,
        format!(" annotation({})", new_placement),
    ))
}

/// Compute the text patch for `SetParameter`.
///
/// Locates the component's modifications list (the `(...)` immediately
/// after the instance name). If absent, inserts a fresh
/// `(param=value)`. If present and the param exists, replaces its
/// value. If present and the param is missing, appends `, param=value`.
fn compute_set_parameter_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    component: &str,
    param: &str,
    value: &str,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let comp = class_def.components.get(component).ok_or_else(|| {
        DocumentError::ValidationFailed(format!(
            "component `{}` not found in class `{}`",
            component, class
        ))
    })?;
    let name_end = comp.name_token.location.end as usize;
    let term = find_statement_terminator(source, name_end).ok_or_else(|| {
        DocumentError::ValidationFailed("component decl has no terminating `;`".into())
    })?;
    // Scan from just after the name token to the terminator looking
    // for `(` before any alphanumeric token (which would indicate an
    // annotation / binding, not modifications).
    let bytes = source.as_bytes();
    let mut i = name_end;
    while i < term {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => {
                i += 1;
            }
            b'(' => {
                // Found modifications list. Locate its matching `)`.
                let mut d = 0;
                let mut k = i;
                let close = loop {
                    match bytes[k] {
                        b'(' | b'{' | b'[' => d += 1,
                        b')' | b'}' | b']' => {
                            d -= 1;
                            if d == 0 {
                                break Some(k);
                            }
                        }
                        _ => {}
                    }
                    k += 1;
                    if k >= term {
                        break None;
                    }
                };
                let close = close.ok_or_else(|| {
                    DocumentError::ValidationFailed(
                        "unterminated `(` in component modifications".into(),
                    )
                })?;
                return Ok(modify_mod_list(source, (i + 1)..close, param, value));
            }
            _ => {
                // No modifications list — insert one right after the name.
                let rendered = format!("({}={})", param, value);
                return Ok((name_end..name_end, rendered));
            }
        }
    }
    // Reached terminator without encountering a `(` — insert fresh list.
    let rendered = format!("({}={})", param, value);
    Ok((name_end..name_end, rendered))
}

// ---------------------------------------------------------------------------
// `__LunCo_PlotNode` vendor annotation helpers
// ---------------------------------------------------------------------------
//
// All four `*PlotNode` ops navigate the same structure:
//
//   class Foo
//     ...
//     annotation(Diagram(graphics={
//       __LunCo_PlotNode(extent=..., signal="...", title="..."),
//       ...
//     }));
//   end Foo;
//
// The helpers below locate each layer (class body → `annotation(...)`
// → `Diagram(...)` → `graphics={...}` → individual entries) using the
// same paren-depth walks the SetPlacement path uses, so the ops compose
// without re-parsing.

/// Find the first `Diagram(...)` call inside any class-level
/// `annotation(...)` block. Walks every `annotation(...)` at depth 0
/// of the class body and returns the first one whose inner contains
/// `Diagram(...)`. Returns `(annotation_span, diagram_outer_span,
/// diagram_inner_span)` so callers can edit the graphics array
/// (innermost), grow the Diagram args (middle), or replace the whole
/// annotation (outer).
fn find_class_diagram(
    source: &str,
    body: Range<usize>,
) -> Option<(Range<usize>, Range<usize>, Range<usize>)> {
    let mut cursor = body.start;
    while cursor < body.end {
        let ann = find_annotation_span(source, cursor..body.end)?;
        let ann_inner_start = ann.start + "annotation(".len();
        let ann_inner_end = ann.end - 1;
        if let Some(d_span) =
            find_named_call_span(source, ann_inner_start..ann_inner_end, "Diagram")
        {
            let d_inner_start = d_span.start + "Diagram(".len();
            let d_inner_end = d_span.end - 1;
            return Some((ann.clone(), d_span, d_inner_start..d_inner_end));
        }
        cursor = ann.end;
    }
    None
}

/// Top-level scan for `<name>(...)` inside `span`. Generalises
/// `find_placement_span`. Matches identifiers that don't continue an
/// adjacent ident (so `__LunCo_PlotNode` is matched but not just
/// `LunCo_PlotNode` in the middle of one).
fn find_named_call_span(source: &str, span: Range<usize>, name: &str) -> Option<Range<usize>> {
    let slice = source.get(span.clone())?;
    let bytes = slice.as_bytes();
    let needle = name.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut i: usize = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                i += 1;
                continue;
            }
            b'(' | b'{' | b'[' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' | b'}' | b']' => {
                depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0
            && i + needle.len() <= bytes.len()
            && &bytes[i..i + needle.len()] == needle
        {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after = i + needle.len();
            let next_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if prev_ok && next_ok {
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    let mut d = 0;
                    let mut k = j;
                    while k < bytes.len() {
                        match bytes[k] {
                            b'(' | b'{' | b'[' => d += 1,
                            b')' | b'}' | b']' => {
                                d -= 1;
                                if d == 0 {
                                    return Some((span.start + i)..(span.start + k + 1));
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    return None;
                }
            }
        }
        i += 1;
    }
    None
}

/// Inside a `Diagram(...)` inner span, find `graphics = {...}` and
/// return the span of the array contents *excluding* the enclosing
/// `{}`. Returns `None` if `graphics=` is missing or its RHS isn't an
/// array literal.
fn find_diagram_graphics_inner(
    source: &str,
    diagram_inner: Range<usize>,
) -> Option<Range<usize>> {
    let slice = source.get(diagram_inner.clone())?;
    let bytes = slice.as_bytes();
    let needle = b"graphics";
    let mut depth: i32 = 0;
    let mut i: usize = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'(' | b'{' | b'[' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' | b'}' | b']' => {
                depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0
            && i + needle.len() <= bytes.len()
            && &bytes[i..i + needle.len()] == needle
        {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after = i + needle.len();
            let next_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if prev_ok && next_ok {
                // Skip ws + `=` + ws.
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'{' {
                        // Find matching `}`.
                        let mut d = 0;
                        let mut k = j;
                        while k < bytes.len() {
                            match bytes[k] {
                                b'(' | b'{' | b'[' => d += 1,
                                b')' | b'}' | b']' => {
                                    d -= 1;
                                    if d == 0 {
                                        // Inner span excludes the
                                        // enclosing braces — the
                                        // splice point for new
                                        // entries goes between the
                                        // braces, after a comma if
                                        // the array is non-empty.
                                        return Some(
                                            (diagram_inner.start + j + 1)
                                                ..(diagram_inner.start + k),
                                        );
                                    }
                                }
                                _ => {}
                            }
                            k += 1;
                        }
                        return None;
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Locate every `__LunCo_PlotNode(...)` entry inside a `graphics={...}`
/// inner span, paired with the `signal=` value extracted from each
/// entry. Returned spans cover the entire call including the trailing
/// `)`. Used by Remove / SetExtent / SetTitle to find the entry by
/// signal path.
fn list_lunco_plot_entries(
    source: &str,
    graphics_inner: Range<usize>,
) -> Vec<(Range<usize>, String)> {
    let mut out = Vec::new();
    let mut cursor = graphics_inner.start;
    while cursor < graphics_inner.end {
        let Some(call) =
            find_named_call_span(source, cursor..graphics_inner.end, "__LunCo_PlotNode")
        else {
            break;
        };
        let inner_start = call.start + "__LunCo_PlotNode(".len();
        let inner_end = call.end - 1;
        let signal = parse_signal_arg(source, inner_start..inner_end).unwrap_or_default();
        out.push((call.clone(), signal));
        cursor = call.end;
    }
    out
}

/// Extract the value of the `signal=` argument from a `__LunCo_PlotNode`
/// call's inner span. Tolerant of argument order — scans top-level
/// for the `signal` identifier followed by `=` and a string literal.
fn parse_signal_arg(source: &str, inner: Range<usize>) -> Option<String> {
    let slice = source.get(inner.clone())?;
    let bytes = slice.as_bytes();
    let needle = b"signal";
    let mut depth: i32 = 0;
    let mut i: usize = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'(' | b'{' | b'[' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' | b'}' | b']' => {
                depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0
            && i + needle.len() <= bytes.len()
            && &bytes[i..i + needle.len()] == needle
        {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after = i + needle.len();
            let next_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if prev_ok && next_ok {
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'"' {
                        // Read until the closing quote, honoring
                        // simple `\"` escapes.
                        let mut k = j + 1;
                        let mut buf = String::new();
                        while k < bytes.len() {
                            let b = bytes[k];
                            if b == b'\\' && k + 1 < bytes.len() {
                                let e = bytes[k + 1];
                                buf.push(match e {
                                    b'"' => '"',
                                    b'\\' => '\\',
                                    other => other as char,
                                });
                                k += 2;
                                continue;
                            }
                            if b == b'"' {
                                return Some(buf);
                            }
                            buf.push(b as char);
                            k += 1;
                        }
                        return None;
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// Append (or update) a `__LunCo_PlotNode(...)` entry in the class's
/// `Diagram(graphics)` array.
///
/// Strategy:
///   - If a `Diagram(graphics={...})` annotation exists and already
///     contains an entry for `plot.signal`, replace the entire entry
///     in place. Otherwise append after the last entry (or as the
///     sole entry if the array is empty).
///   - If a class-level annotation exists but has no `Diagram(...)`,
///     prepend `Diagram(graphics={NEW_ENTRY})` into it.
///   - If no class-level annotation exists, insert a fresh
///     `annotation(Diagram(graphics={NEW_ENTRY}))` just before the
///     class's terminating `end <Name>;`.
fn compute_add_plot_node_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    plot: &pretty::LunCoPlotNodeSpec,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let new_entry = pretty::lunco_plot_node_inner(plot);

    if let Some((_ann, _diagram_outer, diagram_inner)) = find_class_diagram(source, body.clone()) {
        // Diagram(...) exists. Look for graphics={...}.
        if let Some(graphics_inner) =
            find_diagram_graphics_inner(source, diagram_inner.clone())
        {
            let entries = list_lunco_plot_entries(source, graphics_inner.clone());
            if let Some((entry_span, _)) =
                entries.iter().find(|(_, s)| s == &plot.signal)
            {
                // Update existing — full replace of the entry.
                return Ok((entry_span.clone(), new_entry));
            }
            // Append to end of graphics array.
            let trimmed_end = trim_trailing_ws_back(source, graphics_inner.clone());
            let prefix = if entries.is_empty() {
                "".to_string()
            } else {
                ",\n        ".to_string()
            };
            let leading = if entries.is_empty() {
                "\n        "
            } else {
                ""
            };
            let trailing = "\n      ";
            return Ok((
                trimmed_end..graphics_inner.end,
                format!("{prefix}{leading}{new_entry}{trailing}"),
            ));
        }
        // Diagram() with no graphics= argument. Prepend graphics={…} as
        // the first arg.
        let insert = if source[diagram_inner.clone()].trim().is_empty() {
            format!("graphics={{\n        {new_entry}\n      }}")
        } else {
            format!("graphics={{\n        {new_entry}\n      }}, ")
        };
        return Ok((diagram_inner.start..diagram_inner.start, insert));
    }

    // No Diagram annotation. Look for an existing class-level
    // annotation to extend; otherwise create a fresh one before
    // `end <Name>;`.
    if let Some(ann) = find_annotation_span(source, body.clone()) {
        let inner_start = ann.start + "annotation(".len();
        let payload = format!(
            "Diagram(graphics={{\n        {new_entry}\n      }})"
        );
        let insert = if source[inner_start..ann.end - 1].trim().is_empty() {
            payload
        } else {
            format!("{}, ", payload)
        };
        return Ok((inner_start..inner_start, insert));
    }

    // No annotation at all. Insert just before the class's
    // terminating `end <Name>;`. AST `class_def.location.end` lands
    // before that token, so we splice in place.
    let insert_at = class_def.location.end as usize;
    let payload = format!(
        "  annotation(Diagram(graphics={{\n        {new_entry}\n      }}));\n  "
    );
    Ok((insert_at..insert_at, payload))
}

/// Remove the first `__LunCo_PlotNode(...)` entry whose `signal=`
/// matches `signal_path`. Trims a leading or trailing comma so the
/// graphics array stays well-formed.
fn compute_remove_plot_node_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    signal_path: &str,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let (_ann, _diagram_outer, diagram_inner) =
        find_class_diagram(source, body).ok_or_else(|| {
            DocumentError::ValidationFailed(
                "no Diagram(graphics) annotation on class".into(),
            )
        })?;
    let graphics_inner =
        find_diagram_graphics_inner(source, diagram_inner).ok_or_else(|| {
            DocumentError::ValidationFailed("Diagram(...) has no graphics=".into())
        })?;
    let entries = list_lunco_plot_entries(source, graphics_inner.clone());
    let target = entries
        .iter()
        .find(|(_, s)| s == signal_path)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "no plot node with signal `{signal_path}`"
            ))
        })?
        .0
        .clone();
    // Extend the removal span to swallow the comma/whitespace
    // that separates this entry from its neighbour. Prefer the
    // trailing comma (entries earlier in the array) so the array
    // tail stays well-formed; fall back to the leading comma when
    // the entry is last.
    let bytes = source.as_bytes();
    let mut end = target.end;
    let mut k = end;
    while k < graphics_inner.end && (bytes[k].is_ascii_whitespace() || bytes[k] == b',') {
        if bytes[k] == b',' {
            k += 1;
            while k < graphics_inner.end && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            end = k;
            break;
        }
        k += 1;
    }
    if end == target.end {
        // No trailing comma — strip a leading one instead so a
        // residual `, ` doesn't pollute the array.
        let mut s = target.start;
        while s > graphics_inner.start
            && (bytes[s - 1].is_ascii_whitespace() || bytes[s - 1] == b',')
        {
            s -= 1;
        }
        return Ok((s..end, String::new()));
    }
    Ok((target.start..end, String::new()))
}

/// Replace the `extent={{…}}` argument of the `__LunCo_PlotNode` entry
/// matching `signal_path`. Same span-locate logic as `Remove`; this
/// op only ever rewrites within an existing entry, so failure is a
/// hard error rather than a fall-through to "create".
fn compute_set_plot_node_extent_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    signal_path: &str,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
) -> Result<(Range<usize>, String), DocumentError> {
    let entry = locate_plot_entry(source, ast, parsed, class, signal_path)?;
    let inner_start = entry.start + "__LunCo_PlotNode(".len();
    let inner_end = entry.end - 1;
    // Locate the existing `extent` arg span (named-arg form). If
    // missing — unusual; the read-side parser requires it — emit a
    // validation error rather than silently inserting.
    let extent_value = find_named_arg_value_span(source, inner_start..inner_end, "extent")
        .ok_or_else(|| {
            DocumentError::ValidationFailed(
                "plot node has no `extent=` argument".into(),
            )
        })?;
    let new_extent = format!("{{{},{}}},{{{},{}}}", fmt(x1), fmt(y1), fmt(x2), fmt(y2));
    let new_extent = format!("{{{}}}", new_extent);
    Ok((extent_value, new_extent))
}

/// Replace (or insert) the `title=` argument on the matching plot
/// entry. Empty title removes the field entirely.
fn compute_set_plot_node_title_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    signal_path: &str,
    title: &str,
) -> Result<(Range<usize>, String), DocumentError> {
    let entry = locate_plot_entry(source, ast, parsed, class, signal_path)?;
    let inner_start = entry.start + "__LunCo_PlotNode(".len();
    let inner_end = entry.end - 1;
    let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
    let existing = find_named_arg_value_span(source, inner_start..inner_end, "title");
    match (existing, title.is_empty()) {
        (Some(span), true) => {
            // Remove the entire `, title="…"` (or `title="…", `)
            // fragment, eating one separating comma either side.
            let bytes = source.as_bytes();
            let mut start = span.start;
            // Walk back to the `title` keyword start.
            while start > inner_start
                && (bytes[start - 1].is_ascii_whitespace()
                    || bytes[start - 1] == b'=')
            {
                start -= 1;
            }
            let kw = b"title";
            if start >= kw.len() && &bytes[start - kw.len()..start] == kw {
                start -= kw.len();
            }
            // Eat one preceding comma (if any) so we don't leave
            // `, , next` in the args list.
            let mut s = start;
            while s > inner_start
                && (bytes[s - 1].is_ascii_whitespace() || bytes[s - 1] == b',')
            {
                s -= 1;
            }
            Ok((s..span.end, String::new()))
        }
        (Some(span), false) => Ok((span, format!("\"{escaped}\""))),
        (None, true) => {
            // Already absent — emit a no-op edit so the op stays
            // idempotent rather than erroring.
            Ok((inner_end..inner_end, String::new()))
        }
        (None, false) => {
            // Append `, title="…"` just before the closing `)`.
            Ok((inner_end..inner_end, format!(", title=\"{escaped}\"")))
        }
    }
}

/// Find the span of a single plot entry by signal path.
fn locate_plot_entry(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    signal_path: &str,
) -> Result<Range<usize>, DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let (_a, _d, diagram_inner) =
        find_class_diagram(source, body).ok_or_else(|| {
            DocumentError::ValidationFailed(
                "no Diagram(graphics) annotation on class".into(),
            )
        })?;
    let graphics_inner =
        find_diagram_graphics_inner(source, diagram_inner).ok_or_else(|| {
            DocumentError::ValidationFailed("Diagram(...) has no graphics=".into())
        })?;
    list_lunco_plot_entries(source, graphics_inner)
        .into_iter()
        .find_map(|(span, s)| (s == signal_path).then_some(span))
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "no plot node with signal `{signal_path}`"
            ))
        })
}

/// Locate the value-side span of a named argument inside a call's
/// inner span. Returns the byte range covering the argument's value
/// expression, suitable for `EditText` replacement. Skips over
/// nested parens / braces / brackets and string literals.
fn find_named_arg_value_span(
    source: &str,
    inner: Range<usize>,
    name: &str,
) -> Option<Range<usize>> {
    let slice = source.get(inner.clone())?;
    let bytes = slice.as_bytes();
    let needle = name.as_bytes();
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut i: usize = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_str = true;
                i += 1;
                continue;
            }
            b'(' | b'{' | b'[' => {
                depth += 1;
                i += 1;
                continue;
            }
            b')' | b'}' | b']' => {
                depth -= 1;
                i += 1;
                continue;
            }
            _ => {}
        }
        if depth == 0
            && i + needle.len() <= bytes.len()
            && &bytes[i..i + needle.len()] == needle
        {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after = i + needle.len();
            let next_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if prev_ok && next_ok {
                let mut j = after;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'=' {
                    j += 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    // Walk forward at depth 0 of the call until the
                    // next top-level comma or end of inner.
                    let value_start = j;
                    let mut d = 0;
                    let mut in_s = false;
                    while j < bytes.len() {
                        let b = bytes[j];
                        if in_s {
                            if b == b'\\' && j + 1 < bytes.len() {
                                j += 2;
                                continue;
                            }
                            if b == b'"' {
                                in_s = false;
                            }
                            j += 1;
                            continue;
                        }
                        match b {
                            b'"' => in_s = true,
                            b'(' | b'{' | b'[' => d += 1,
                            b')' | b'}' | b']' => d -= 1,
                            b',' if d == 0 => break,
                            _ => {}
                        }
                        j += 1;
                    }
                    // Trim trailing whitespace from the value span.
                    let mut end = j;
                    while end > value_start
                        && bytes[end - 1].is_ascii_whitespace()
                    {
                        end -= 1;
                    }
                    return Some((inner.start + value_start)..(inner.start + end));
                }
            }
        }
        i += 1;
    }
    None
}

/// Trim trailing whitespace at the end of `span`, returning the
/// adjusted end position (the splice insertion point that places new
/// content cleanly without doubling spaces).
fn trim_trailing_ws_back(source: &str, span: Range<usize>) -> usize {
    let bytes = source.as_bytes();
    let mut end = span.end;
    while end > span.start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

/// Locate the byte span of the i-th `Text(...)` call inside the
/// class's `Diagram(graphics)` array. Text-only counter — items of
/// other kinds (Rectangle, Line, etc.) don't increment `index`.
fn locate_diagram_text_entry(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    target_index: usize,
) -> Result<Range<usize>, DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let (_a, _d, diagram_inner) =
        find_class_diagram(source, body).ok_or_else(|| {
            DocumentError::ValidationFailed(
                "no Diagram(graphics) annotation on class".into(),
            )
        })?;
    let graphics_inner =
        find_diagram_graphics_inner(source, diagram_inner).ok_or_else(|| {
            DocumentError::ValidationFailed("Diagram(...) has no graphics=".into())
        })?;
    let mut cursor = graphics_inner.start;
    let mut text_idx: usize = 0;
    while cursor < graphics_inner.end {
        let Some(call) = find_named_call_span(source, cursor..graphics_inner.end, "Text")
        else {
            break;
        };
        if text_idx == target_index {
            return Ok(call);
        }
        text_idx += 1;
        cursor = call.end;
    }
    Err(DocumentError::ValidationFailed(format!(
        "no Text entry at index {target_index} in Diagram(graphics)"
    )))
}

fn compute_set_diagram_text_extent_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    index: usize,
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
) -> Result<(Range<usize>, String), DocumentError> {
    let entry = locate_diagram_text_entry(source, ast, parsed, class, index)?;
    let inner_start = entry.start + "Text(".len();
    let inner_end = entry.end - 1;
    let extent_value = find_named_arg_value_span(source, inner_start..inner_end, "extent")
        .ok_or_else(|| {
            DocumentError::ValidationFailed("Text has no `extent=` argument".into())
        })?;
    let new_extent = format!(
        "{{{{{},{}}},{{{},{}}}}}",
        fmt(x1),
        fmt(y1),
        fmt(x2),
        fmt(y2)
    );
    Ok((extent_value, new_extent))
}

fn compute_set_diagram_text_string_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    index: usize,
    text: &str,
) -> Result<(Range<usize>, String), DocumentError> {
    let entry = locate_diagram_text_entry(source, ast, parsed, class, index)?;
    let inner_start = entry.start + "Text(".len();
    let inner_end = entry.end - 1;
    let value = find_named_arg_value_span(source, inner_start..inner_end, "textString")
        .ok_or_else(|| {
            DocumentError::ValidationFailed("Text has no `textString=` argument".into())
        })?;
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    Ok((value, format!("\"{escaped}\"")))
}

fn compute_remove_diagram_text_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    index: usize,
) -> Result<(Range<usize>, String), DocumentError> {
    let entry = locate_diagram_text_entry(source, ast, parsed, class, index)?;
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let (_a, _d, diagram_inner) =
        find_class_diagram(source, body).ok_or_else(|| {
            DocumentError::ValidationFailed(
                "no Diagram(graphics) annotation on class".into(),
            )
        })?;
    let graphics_inner =
        find_diagram_graphics_inner(source, diagram_inner).ok_or_else(|| {
            DocumentError::ValidationFailed("Diagram(...) has no graphics=".into())
        })?;
    // Same trailing-comma-then-leading-comma trim the plot node
    // remove uses: keep the array well-formed regardless of
    // whether the entry was first / middle / last.
    let bytes = source.as_bytes();
    let mut end = entry.end;
    let mut k = end;
    while k < graphics_inner.end && (bytes[k].is_ascii_whitespace() || bytes[k] == b',') {
        if bytes[k] == b',' {
            k += 1;
            while k < graphics_inner.end && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            end = k;
            break;
        }
        k += 1;
    }
    if end == entry.end {
        let mut s = entry.start;
        while s > graphics_inner.start
            && (bytes[s - 1].is_ascii_whitespace() || bytes[s - 1] == b',')
        {
            s -= 1;
        }
        return Ok((s..end, String::new()));
    }
    Ok((entry.start..end, String::new()))
}

/// Render a coordinate as it appears in `extent={{…}}`. Integers
/// emit without a trailing `.0` so common diagram positions stay
/// short and stable across round-trips.
fn fmt(v: f32) -> String {
    if v.fract() == 0.0 && v.abs() < 1e10 {
        format!("{}", v as i64)
    } else {
        format!("{}", v)
    }
}

/// Helper: emit the patch that either updates or appends `param=value`
/// within the top-level modification list occupying `inner_span`
/// (exclusive of the outer parens).
fn modify_mod_list(
    source: &str,
    inner_span: Range<usize>,
    param: &str,
    value: &str,
) -> (Range<usize>, String) {
    let bytes = source.as_bytes();
    // Walk the list at depth 0, splitting entries by `,`. For each
    // entry, check if it starts (after whitespace) with `param` and is
    // followed by `=` or `(` (modification or nested modification).
    let start = inner_span.start;
    let end = inner_span.end;
    let mut entry_start = start;
    let mut d = 0;
    let mut i = start;
    while i < end {
        let c = bytes[i];
        match c {
            b'(' | b'{' | b'[' => d += 1,
            b')' | b'}' | b']' => d -= 1,
            b',' if d == 0 => {
                if let Some(patch) = match_entry(source, entry_start..i, param, value) {
                    return patch;
                }
                entry_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    // Final entry.
    if let Some(patch) = match_entry(source, entry_start..end, param, value) {
        return patch;
    }
    // Not found — append.
    let trimmed_end = {
        let mut e = end;
        while e > start
            && (source.as_bytes()[e - 1] == b' '
                || source.as_bytes()[e - 1] == b'\t'
                || source.as_bytes()[e - 1] == b'\n'
                || source.as_bytes()[e - 1] == b'\r')
        {
            e -= 1;
        }
        e
    };
    let insertion = if trimmed_end == start {
        format!("{}={}", param, value)
    } else {
        format!(", {}={}", param, value)
    };
    (trimmed_end..trimmed_end, insertion)
}

/// If `entry` (a slice of the modifications list) names `param`, return
/// the patch to replace its right-hand value with `value`. Otherwise
/// return `None`.
fn match_entry(
    source: &str,
    entry: Range<usize>,
    param: &str,
    value: &str,
) -> Option<(Range<usize>, String)> {
    let slice = source.get(entry.clone())?;
    // Skip leading whitespace.
    let pre_ws = slice.chars().take_while(|c| c.is_whitespace()).count();
    let name_start = entry.start + pre_ws;
    let remainder = source.get(name_start..entry.end)?;
    if !remainder.starts_with(param) {
        return None;
    }
    // Ensure the next char is an identifier boundary.
    let after_idx = name_start + param.len();
    let after_char = source.as_bytes().get(after_idx).copied();
    if matches!(after_char, Some(b'=') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')) {
        // Find the `=` and replace everything after it (trimmed) up to
        // entry end.
        let eq_pos = source.get(after_idx..entry.end)?.find('=')?;
        let value_start = after_idx + eq_pos + 1;
        // Strip trailing whitespace from entry end for a clean replace.
        let mut value_end = entry.end;
        while value_end > value_start {
            let b = source.as_bytes()[value_end - 1];
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                value_end -= 1;
            } else {
                break;
            }
        }
        let replacement = format!("{}{}", if source.as_bytes()[value_start] == b' ' { "" } else { " " }, value);
        return Some((value_start..value_end, replacement));
    }
    None
}

// ---------------------------------------------------------------------------
// Layer 2 writers — class / variable / equation / graphic / experiment
// ---------------------------------------------------------------------------

/// Insert an empty class block inside `parent` (or top level if empty).
fn compute_add_class_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    parent: &str,
    name: &str,
    kind: pretty::ClassKindSpec,
    description: &str,
    partial: bool,
) -> Result<(Range<usize>, String), DocumentError> {
    if name.is_empty() {
        return Err(DocumentError::ValidationFailed("class name is empty".into()));
    }
    let block = pretty::class_block_empty(name, kind, description, partial);
    if parent.is_empty() {
        // Top-level: append at end of file with a leading blank line if needed.
        let prefix = if source.is_empty() || source.ends_with("\n\n") {
            String::new()
        } else if source.ends_with('\n') {
            "\n".to_string()
        } else {
            "\n\n".to_string()
        };
        let pos = source.len();
        return Ok((pos..pos, format!("{prefix}{block}")));
    }
    let parent_def = resolve_class(ast, parsed, parent)?;
    let end_byte = parent_def
        .end_name_token
        .as_ref()
        .map(|t| t.location.start as usize)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "parent class `{}` has no `end` clause location",
                parent
            ))
        })?;
    let line_start = line_start_byte(source, end_byte);
    // Indent the block's own first line and `end`-line with the parent's
    // body indent (one `options().indent` level).
    let body_indent = pretty::options().indent;
    let indented: String = block
        .lines()
        .enumerate()
        .map(|(i, l)| {
            if l.is_empty() && i + 1 == block.lines().count() {
                String::new()
            } else {
                format!("{}{}\n", body_indent, l)
            }
        })
        .collect();
    Ok((line_start..line_start, indented))
}

/// Remove a class by qualified name. Span = first token of the class
/// header through the trailing `;` of `end <Name>;`, line-extended.
fn compute_remove_class_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    qualified: &str,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, qualified)?;
    let raw_start = class_def.location.start as usize;
    // class_def.location.end stops before `end`; advance to past the
    // terminating `;` of `end <Name>;`.
    let end_token_start = class_def
        .end_name_token
        .as_ref()
        .map(|t| t.location.start as usize)
        .unwrap_or(class_def.location.end as usize);
    let term = find_statement_terminator(source, end_token_start)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "could not find `;` terminating class `{qualified}`"
            ))
        })?;
    let span = extend_span_to_whole_lines(source, raw_start..(term + 1));
    Ok((span, String::new()))
}

/// Insert a short-class definition inside `parent` (or top level).
fn compute_add_short_class_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    parent: &str,
    name: &str,
    kind: pretty::ClassKindSpec,
    base: &str,
    prefixes: &[String],
    modifications: &[(String, String)],
) -> Result<(Range<usize>, String), DocumentError> {
    if name.is_empty() || base.is_empty() {
        return Err(DocumentError::ValidationFailed(
            "short class name or base is empty".into(),
        ));
    }
    let line = pretty::short_class_decl(name, kind, base, prefixes, modifications);
    if parent.is_empty() {
        let prefix = if source.is_empty() || source.ends_with('\n') {
            String::new()
        } else {
            "\n".to_string()
        };
        let pos = source.len();
        // Strip the `options().indent` prefix so top-level lines aren't
        // body-indented.
        let body_indent = pretty::options().indent;
        let unindented = line.strip_prefix(&body_indent).unwrap_or(&line).to_string();
        return Ok((pos..pos, format!("{prefix}{unindented}")));
    }
    let parent_def = resolve_class(ast, parsed, parent)?;
    let end_byte = parent_def
        .end_name_token
        .as_ref()
        .map(|t| t.location.start as usize)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "parent class `{}` has no `end` clause location",
                parent
            ))
        })?;
    let line_start = line_start_byte(source, end_byte);
    Ok((line_start..line_start, line))
}

/// Insert a variable declaration into a class body, just before the
/// equation/algorithm section or `end <Name>`.
fn compute_add_variable_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    decl: &pretty::VariableDecl,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let insertion_byte = class_section_insertion_point(class_def).ok_or_else(|| {
        DocumentError::ValidationFailed(format!(
            "could not locate insertion point in class `{class}`"
        ))
    })?;
    let line_start = line_start_byte(source, insertion_byte);
    Ok((line_start..line_start, pretty::variable_decl(decl)))
}

/// Append an equation to a class equation section, creating one if needed.
fn compute_add_equation_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    eq: &pretty::EquationDecl,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let end_name_byte = class_def
        .end_name_token
        .as_ref()
        .map(|t| t.location.start as usize)
        .ok_or_else(|| {
            DocumentError::ValidationFailed(format!(
                "class `{class}` has no `end` clause location"
            ))
        })?;
    let end_line_start = line_start_byte(source, end_name_byte);
    let eq_line = pretty::equation_decl(eq);
    let replacement = if class_def.equation_keyword.is_some() {
        eq_line
    } else {
        format!("equation\n{}", eq_line)
    };
    Ok((end_line_start..end_line_start, replacement))
}

/// Append a graphic to `Icon(graphics={...})` or `Diagram(graphics={...})`,
/// creating the wrapper if needed. Mirrors `compute_add_plot_node_patch`'s
/// shape for Diagram, generalised to accept the layer name.
fn compute_add_named_graphic_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    layer: &str,
    graphic: &pretty::GraphicSpec,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let new_entry = pretty::graphic_inner(graphic);

    if let Some(layer_inner) = find_class_named_graphics_layer(source, body.clone(), layer) {
        if let Some(graphics_inner) = find_diagram_graphics_inner(source, layer_inner.clone()) {
            // graphics={...} exists — append.
            let array_text = &source[graphics_inner.clone()];
            let prefix = if array_text.trim().is_empty() { "" } else { ",\n        " };
            let leading = if array_text.trim().is_empty() { "\n        " } else { "" };
            let trailing = "\n      ";
            let trimmed_end = trim_trailing_ws_back(source, graphics_inner.clone());
            return Ok((
                trimmed_end..graphics_inner.end,
                format!("{prefix}{leading}{new_entry}{trailing}"),
            ));
        }
        // <Layer>() with no graphics= argument — prepend.
        let insert = if source[layer_inner.clone()].trim().is_empty() {
            format!("graphics={{\n        {new_entry}\n      }}")
        } else {
            format!("graphics={{\n        {new_entry}\n      }}, ")
        };
        return Ok((layer_inner.start..layer_inner.start, insert));
    }
    // No <Layer>(...) yet. Look for an existing class-level annotation
    // to extend.
    if let Some(ann) = find_annotation_span(source, body.clone()) {
        let inner_start = ann.start + "annotation(".len();
        let payload = format!(
            "{layer}(graphics={{\n        {new_entry}\n      }})"
        );
        let insert = if source[inner_start..ann.end - 1].trim().is_empty() {
            payload
        } else {
            format!("{}, ", payload)
        };
        return Ok((inner_start..inner_start, insert));
    }
    // No annotation at all — insert one before `end <Name>;`.
    let insert_at = class_def.location.end as usize;
    let payload = format!(
        "  annotation({layer}(graphics={{\n        {new_entry}\n      }}));\n  "
    );
    Ok((insert_at..insert_at, payload))
}

/// Locate `<layer>(...)` (Icon or Diagram) inside any class-level
/// annotation. Returns the byte span between the parens, *excluding*
/// the parens themselves. Counterpart to `find_class_diagram` that's
/// generalised over the layer keyword.
fn find_class_named_graphics_layer(
    source: &str,
    body: Range<usize>,
    layer: &str,
) -> Option<Range<usize>> {
    let mut cursor = body.start;
    while cursor < body.end {
        let ann = find_annotation_span(source, cursor..body.end)?;
        let ann_inner_start = ann.start + "annotation(".len();
        let ann_inner_end = ann.end - 1;
        if let Some(span) = find_named_call_span(source, ann_inner_start..ann_inner_end, layer) {
            let inner_start = span.start + layer.len() + 1; // `<layer>(`
            let inner_end = span.end - 1;                   // before `)`
            return Some(inner_start..inner_end);
        }
        cursor = ann.end;
    }
    None
}

/// Set or insert the `experiment(...)` annotation on a class.
fn compute_set_experiment_patch(
    source: &str,
    ast: &AstCache,
    parsed: &rumoca_session::parsing::ast::StoredDefinition,
    class: &str,
    start_time: f64,
    stop_time: f64,
    tolerance: f64,
    interval: f64,
) -> Result<(Range<usize>, String), DocumentError> {
    let class_def = resolve_class(ast, parsed, class)?;
    let body = (class_def.location.start as usize)..(class_def.location.end as usize);
    let new_inner = pretty::experiment_inner(start_time, stop_time, tolerance, interval);

    if let Some(ann) = find_annotation_span(source, body.clone()) {
        let ann_inner_start = ann.start + "annotation(".len();
        let ann_inner_end = ann.end - 1;
        if let Some(span) = find_named_call_span(source, ann_inner_start..ann_inner_end, "experiment") {
            // Replace whole experiment(...) call.
            return Ok((span, new_inner));
        }
        // Prepend the experiment(...) entry to the existing annotation.
        let insert = if source[ann_inner_start..ann_inner_end].trim().is_empty() {
            new_inner
        } else {
            format!("{new_inner}, ")
        };
        return Ok((ann_inner_start..ann_inner_start, insert));
    }
    // No annotation — create one before `end <Name>;`.
    let insert_at = class_def.location.end as usize;
    Ok((
        insert_at..insert_at,
        format!("  annotation({new_inner});\n  "),
    ))
}

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
