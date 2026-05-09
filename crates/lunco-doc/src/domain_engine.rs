//! Domain engine abstraction.
//!
//! A [`DomainEngine`] is the in-process owner of a domain's parser session
//! (e.g. `rumoca_session::Session` for Modelica, `pxr::UsdStage` for USD)
//! and projects each open document into a UI-friendly [`DomainEngine::Index`].
//!
//! UI code reads the Index, never the engine's internal AST. Edits are
//! applied as typed [`DocumentOp`](crate::DocumentOp)s; the engine returns
//! the inverse op for undo.
//!
//! One engine instance per process per domain — the engine owns cross-file
//! state (symbol tables, fingerprint caches) so per-document ops stay cheap.
//!
//! ## Why a trait?
//!
//! Two implementers today (Modelica, USD) plus future SysML. The trait keeps
//! workbench callers domain-agnostic: panels iterate engines, render their
//! Indexes, dispatch ops uniformly. Domain-specific behavior stays inside
//! each engine impl.

use crate::{DocumentId, DocumentOp, SymbolPath};

// ─────────────────────────────────────────────────────────────────────────────
// Stable per-AST-node identity
// ─────────────────────────────────────────────────────────────────────────────

/// Stable per-AST-node identifier within one document.
///
/// Engines define their own scheme — Modelica uses a string like
/// `"Rocket.engine|component|thrust"`; USD uses prim+attr paths.
/// Stability across re-parses is what lets the [`DomainEngine::Index`]
/// reconcile incrementally instead of rebuilding from scratch.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord,
    serde::Serialize, serde::Deserialize,
)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Cross-document references
// ─────────────────────────────────────────────────────────────────────────────

/// A reference from a node in this document to a symbol that may live in
/// another document.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SymbolRef {
    /// Fully-qualified target symbol.
    pub path: SymbolPath,
    /// Node in *this* document that holds the reference.
    pub from_node: NodeId,
}

/// A resolved cross-document reference — the document and node a
/// [`SymbolPath`] resolves to.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ResolvedRef {
    pub doc: DocumentId,
    pub node: NodeId,
}

// ─────────────────────────────────────────────────────────────────────────────
// Diagnostics
// ─────────────────────────────────────────────────────────────────────────────

/// Half-open byte range in document source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TextRange {
    pub start: usize,
    pub end: usize,
}

impl TextRange {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub range: Option<TextRange>,
}

// ─────────────────────────────────────────────────────────────────────────────
// DomainEngine
// ─────────────────────────────────────────────────────────────────────────────

/// A domain-specific editing engine.
///
/// Owns the parser/session for one modeling domain (Modelica, USD, SysML)
/// across all open documents in that domain. Projects each document into a
/// domain-specific [`Self::Index`] type that UI consumes.
///
/// Edit pipeline:
///
/// 1. UI gesture → typed [`Self::Op`].
/// 2. Engine `apply`s op: optimistically patches the Index, returns inverse.
/// 3. (Async) engine reparses authoritative source on a debounce, reconciles
///    Index against the new AST. UI never blocks.
///
/// # TODO: AST-canonical input (roadmap step 4b)
///
/// The current `open(source)` and apply-then-reparse pipeline accepts
/// **source** as the engine's primary input. The concrete `ModelicaEngine`
/// has already moved past this — its public surface is
/// [`upsert_document_with_ast`](../../../lunco_modelica/engine/struct.ModelicaEngine.html#method.upsert_document_with_ast),
/// not a source-taking method. The source-taking convenience there is
/// `#[deprecated]`. The principle: **engine input format is AST. To get an
/// AST you parse explicitly; the parse cost stays visible at the call
/// site.** See `lunco-modelica/src/document.rs::FreshAst` for the
/// producer-side encoding of the same invariant.
///
/// When the second [`DomainEngine`] impl arrives (USD, SysML, …), this
/// trait should be reshaped so source isn't accepted at all:
///
/// ```ignore
/// pub trait DomainEngine {
///     type Op: DocumentOp;
///     type Index;
///     /// Domain-specific parsed-input type — AST/StoredDef for Modelica,
///     /// Stage for USD, SyntaxTree for SysML.
///     type ParsedInput;
///
///     fn open(&mut self, id: DocumentId, parsed: Self::ParsedInput)
///         -> Result<(), DomainEngineError>;
///     // ... apply/index/diagnostics unchanged
/// }
/// ```
///
/// `open(source: String)` and the "(Async) engine reparses authoritative
/// source" leg of the pipeline above then move into a separate
/// `TextEditDriver` that's wired only when a doc is in code-editor /
/// text-edit mode — exactly mirroring the
/// `FreshAst::Mutated` / `FreshAst::TextEdit` split on the producer side.
/// Until that second impl exists, the trait stays source-taking to avoid
/// API churn for one user, but the principle is documented here so the
/// next implementer doesn't repeat the source-as-canonical mistake the
/// Modelica side just unwound.
pub trait DomainEngine: Send + Sync + 'static {
    /// The op type this engine accepts.
    type Op: DocumentOp;

    /// The Index type projected per open document. UI reads this.
    type Index;

    /// Open a document with initial source. After success, [`Self::index`]
    /// returns Some for this id.
    ///
    /// **TODO** (roadmap step 4b): replace the `source: String` input
    /// with a domain-specific `ParsedInput` associated type so the
    /// engine never accepts un-parsed text. See trait-level docs for
    /// the migration shape. Today's signature is kept until a second
    /// engine impl arrives.
    fn open(&mut self, id: DocumentId, source: String) -> Result<(), DomainEngineError>;

    /// Close a document. Releases per-doc resources.
    fn close(&mut self, id: DocumentId);

    /// Apply an op, returning the inverse for undo.
    ///
    /// Engines apply optimistically — Index is updated synchronously for
    /// instant UI feedback; authoritative reparse is scheduled async.
    fn apply(&mut self, id: DocumentId, op: Self::Op) -> Result<Self::Op, DomainEngineError>;

    /// Read-only access to the Index. Hot path; must be cheap.
    fn index(&self, id: DocumentId) -> Option<&Self::Index>;

    /// Render the document to source text (used for Save).
    fn print(&self, id: DocumentId) -> Option<String>;

    /// Diagnostics for this document.
    fn diagnostics(&self, id: DocumentId) -> &[Diagnostic];

    /// Symbols this document defines, fully-qualified.
    /// Used by [`crate::RefIndex`] to maintain the cross-doc reference table.
    fn defines(&self, id: DocumentId) -> &[SymbolPath];

    /// References emanating from this document.
    /// Used by [`crate::RefIndex`] to track cross-doc dependents.
    fn refs_out(&self, id: DocumentId) -> &[SymbolRef];
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DomainEngineError {
    NotOpen(DocumentId),
    InvalidOp(String),
    Apply(String),
    Parse(String),
}

impl std::fmt::Display for DomainEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DomainEngineError::NotOpen(id) => write!(f, "document {} not open", id),
            DomainEngineError::InvalidOp(m) => write!(f, "invalid op: {}", m),
            DomainEngineError::Apply(m) => write!(f, "apply failed: {}", m),
            DomainEngineError::Parse(m) => write!(f, "parse failed: {}", m),
        }
    }
}

impl std::error::Error for DomainEngineError {}
