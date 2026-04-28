//! `UsdDocument` — the canonical Document representation of one USD
//! source file (`.usda` for now; `.usdc` deferred).
//!
//! Mirrors the shape of [`lunco_modelica::ModelicaDocument`]:
//! source text is canonical, ops mutate text, generation bumps on
//! every change, last-saved-generation gates the dirty flag, a bounded
//! ring of recent changes lets views catch up without polling.
//!
//! ## Why text-canonical
//!
//! USD `.usda` is plain text. Treating the text as canonical (rather
//! than the parsed `TextReader`) gives us:
//!
//! - Lossless round-trip with external USD tools (Omniverse, USDView,
//!   Blender) — comments and formatting survive untouched until an op
//!   actually rewrites their byte range.
//! - One mutation path: every op funnels through a `(range, replacement)`
//!   patch, same as Modelica. No parallel "in-memory tree" representation
//!   to keep in sync.
//! - Trivial Phase 1: a `ReplaceSource` op is enough to plumb the
//!   `Document` trait + undo/redo without committing to a prim-level op
//!   shape yet (that lands in Phase 5).
//!
//! ## Edit target
//!
//! Per the Omniverse pattern, every `UsdOp` carries an `edit_target:
//! LayerId` so future composition-aware editing can name *which layer*
//! receives an opinion. Phase 1 only knows about the root layer
//! ([`LayerId::root`]); the field exists so Phase 5 can extend without
//! repainting the type.

use std::collections::VecDeque;
use std::ops::Range;

use lunco_doc::{Document, DocumentError, DocumentId, DocumentOp, DocumentOrigin};

/// How many recent changes to keep in the per-document ring buffer.
///
/// Views consume the suffix via [`UsdDocument::changes_since`]; 256 is
/// generous for realistic edit cadences without growing unbounded.
const CHANGE_HISTORY_CAPACITY: usize = 256;

// ─────────────────────────────────────────────────────────────────────
// LayerId — names a layer in a stage's layer stack
// ─────────────────────────────────────────────────────────────────────

/// Identifies one layer in a USD stage's layer stack.
///
/// In Phase 1 every document is a single root layer and every op
/// targets [`LayerId::root`]. The newtype exists now so Phase 5
/// (sublayer-aware editing) can extend without changing op shapes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LayerId(String);

impl LayerId {
    /// The root layer of a stage — the file the document was opened from.
    pub fn root() -> Self {
        Self("@root@".to_string())
    }

    /// Wrap an arbitrary layer identifier (path or anonymous handle).
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The raw identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True when this id refers to the document's root layer.
    pub fn is_root(&self) -> bool {
        self.0 == "@root@"
    }
}

impl Default for LayerId {
    fn default() -> Self {
        Self::root()
    }
}

// ─────────────────────────────────────────────────────────────────────
// UsdChange — Omniverse-style change notification
// ─────────────────────────────────────────────────────────────────────

/// Coarse-grained change classification, modelled on USD's
/// `Tf::Notice` split between resync (structural) and info-only
/// (attribute value) changes.
///
/// Views subscribe to the kinds they care about — the prim-tree
/// browser only rebuilds on `Resync`; the property inspector reacts
/// to `InfoOnly` for the selected prim. This is the plumbing that
/// keeps frame discipline (see `AGENTS.md` §7) when a single attr
/// edit happens on a 100k-prim stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsdChange {
    /// Structural change: prim added, removed, renamed, or moved.
    /// Forces a tree rebuild.
    Resync {
        /// Prim path (or `/` for whole-stage replacement).
        path: String,
    },
    /// Attribute value changed; tree shape unchanged.
    InfoOnly {
        /// Prim path whose attribute changed.
        path: String,
        /// Attribute name (e.g. `xformOp:translate`).
        attr: String,
    },
    /// Whole source replaced — every observer should refresh.
    /// Used by `ReplaceSource` and Save-As round-trips.
    FullReload,
}

// ─────────────────────────────────────────────────────────────────────
// UsdOp — typed mutation
// ─────────────────────────────────────────────────────────────────────

/// A typed, reversible mutation to a [`UsdDocument`].
///
/// Phase 1 ships a single op variant — `ReplaceSource` — which is
/// enough to plumb the `Document` trait, exercise undo/redo, and let
/// the API/UI dispatch text edits via the typed-command surface.
/// Prim-level ops (`AddPrim`, `RemovePrim`, `SetAttribute`,
/// `SetTransform`) land in Phase 5 alongside the Sdf-style writer.
///
/// `edit_target` follows the Omniverse pattern: every op names which
/// layer in the stage's layer stack receives the opinion. Today only
/// [`LayerId::root`] is meaningful; the field is present so future
/// layered edits don't require an op-shape rewrite.
#[derive(Debug, Clone)]
pub enum UsdOp {
    /// Replace the entire source buffer with `text`. Inverse is the
    /// previous source as another `ReplaceSource`. Mirrors Modelica's
    /// `EditText` — coarse-grained but always valid.
    ReplaceSource {
        /// Layer to write to. Today: always [`LayerId::root`].
        edit_target: LayerId,
        /// New full source for the layer.
        text: String,
    },
}

impl DocumentOp for UsdOp {}

// ─────────────────────────────────────────────────────────────────────
// UsdDocument
// ─────────────────────────────────────────────────────────────────────

/// The canonical Document representation of one USD source file.
///
/// Owns the source text + a [`DocumentOrigin`] (where it came from,
/// whether it can be saved) + a generation counter that bumps on every
/// successful op. Parsed-stage caching is **deferred to Phase 4** —
/// the document layer holds text only; rendering/inspection layers
/// drive the parse and cache the `TextReader` themselves.
#[derive(Debug, Clone)]
pub struct UsdDocument {
    id: DocumentId,
    source: String,
    generation: u64,
    origin: DocumentOrigin,
    /// Generation at which the document was last persisted to disk.
    /// `None` = never saved (freshly created in-memory); `Some(g)` =
    /// last saved at generation `g`. Drives `is_dirty`.
    last_saved_generation: Option<u64>,
    /// Ring buffer of `(generation_after_change, change)` for catch-up
    /// reads. See [`changes_since`](Self::changes_since).
    changes: VecDeque<(u64, UsdChange)>,
}

impl UsdDocument {
    /// Build a fresh in-memory `UsdDocument` with the given source as
    /// an Untitled document. Starts dirty (never-saved).
    pub fn new(id: DocumentId, source: impl Into<String>) -> Self {
        Self::with_origin(
            id,
            source,
            DocumentOrigin::untitled(format!("Untitled-{}.usda", id.raw())),
        )
    }

    /// Build a `UsdDocument` with an explicit origin.
    ///
    /// On-disk origins start clean (source assumed to match disk at
    /// generation 0). Untitled origins start dirty.
    pub fn with_origin(
        id: DocumentId,
        source: impl Into<String>,
        origin: DocumentOrigin,
    ) -> Self {
        let source = source.into();
        let last_saved_generation = match &origin {
            DocumentOrigin::File { .. } => Some(0),
            DocumentOrigin::Untitled { .. } => None,
        };
        Self {
            id,
            source,
            generation: 0,
            origin,
            last_saved_generation,
            changes: VecDeque::with_capacity(CHANGE_HISTORY_CAPACITY),
        }
    }

    /// The current source text. Canonical representation; everything
    /// else (parsed stage, prim tree, viewport entities) is derived.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Where this document came from (drives save behaviour, tab
    /// title, read-only badge).
    pub fn origin(&self) -> &DocumentOrigin {
        &self.origin
    }

    /// Replace the origin in-place. Used by Save-As to rebind an
    /// Untitled document to a fresh on-disk path; bumps the
    /// last-saved-generation marker so the dirty flag clears.
    pub fn set_origin(&mut self, origin: DocumentOrigin) {
        self.origin = origin;
        self.last_saved_generation = Some(self.generation);
    }

    /// Whether the document has unsaved changes.
    ///
    /// Untitled docs are always dirty; on-disk docs are dirty iff the
    /// current generation is past the last-saved one.
    pub fn is_dirty(&self) -> bool {
        match self.last_saved_generation {
            None => true,
            Some(g) => self.generation > g,
        }
    }

    /// Mark the current state as the last-saved baseline. Called by
    /// the Save command after a successful disk write.
    pub fn mark_saved(&mut self) {
        self.last_saved_generation = Some(self.generation);
    }

    /// Suffix of the change ring strictly after `since_generation`.
    ///
    /// Views track their last-observed generation and pull only the
    /// new tail each frame, sidestepping per-frame full rescans
    /// (`AGENTS.md` §7.1).
    pub fn changes_since(&self, since_generation: u64) -> impl Iterator<Item = (u64, &UsdChange)> {
        self.changes
            .iter()
            .filter(move |(g, _)| *g > since_generation)
            .map(|(g, c)| (*g, c))
    }

    // ─── internal ──────────────────────────────────────────────────────

    /// Core mutation path. All ops funnel through here so generation
    /// bumps, change emission, and ring trimming happen in exactly one
    /// place.
    fn apply_text_replace(
        &mut self,
        range: Range<usize>,
        replacement: String,
        change: UsdChange,
    ) -> Result<UsdOp, DocumentError> {
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
        // Capture the previous source for the inverse op before mutating.
        let previous = self.source.clone();
        self.source.replace_range(range, &replacement);
        self.generation += 1;
        if self.changes.len() == CHANGE_HISTORY_CAPACITY {
            self.changes.pop_front();
        }
        self.changes.push_back((self.generation, change));
        // Phase 1: every op replaces full source, so the inverse is the
        // verbatim previous source as another ReplaceSource. When
        // typed prim ops land in Phase 5 they'll compute tighter
        // inverses (e.g. `RemovePrim` ↔ `AddPrim`).
        Ok(UsdOp::ReplaceSource {
            edit_target: LayerId::root(),
            text: previous,
        })
    }
}

impl Document for UsdDocument {
    type Op = UsdOp;

    fn id(&self) -> DocumentId {
        self.id
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn apply(&mut self, op: Self::Op) -> Result<Self::Op, DocumentError> {
        // The document is the single source of truth for its own
        // mutability — every dispatch path (UI, API, MCP, scripts)
        // gets the same `ReadOnly` error and surfaces it through
        // their normal error paths. No band-aid pre-checks in panels.
        if !self.origin.accepts_mutations() {
            return Err(DocumentError::ReadOnly);
        }
        match op {
            UsdOp::ReplaceSource { edit_target, text } => {
                if !edit_target.is_root() {
                    return Err(DocumentError::ValidationFailed(format!(
                        "edit target {:?} not supported in Phase 1 (root only)",
                        edit_target
                    )));
                }
                let range = 0..self.source.len();
                self.apply_text_replace(range, text, UsdChange::FullReload)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lunco_doc::{DocumentHost, Mutation};

    const TINY_USDA: &str = "#usda 1.0\n(\n    defaultPrim = \"World\"\n)\n\ndef Xform \"World\"\n{\n}\n";

    #[test]
    fn untitled_starts_dirty_and_writable() {
        let doc = UsdDocument::new(DocumentId::new(1), TINY_USDA);
        assert!(doc.is_dirty());
        assert!(doc.origin().accepts_mutations());
        assert_eq!(doc.generation(), 0);
    }

    #[test]
    fn from_file_origin_starts_clean() {
        let doc = UsdDocument::with_origin(
            DocumentId::new(2),
            TINY_USDA,
            DocumentOrigin::writable_file("/tmp/scene.usda"),
        );
        assert!(!doc.is_dirty());
    }

    #[test]
    fn readonly_origin_rejects_ops() {
        let mut doc = UsdDocument::with_origin(
            DocumentId::new(3),
            TINY_USDA,
            DocumentOrigin::readonly_file("/tmp/scene.usda"),
        );
        let err = doc
            .apply(UsdOp::ReplaceSource {
                edit_target: LayerId::root(),
                text: "broken".to_string(),
            })
            .unwrap_err();
        assert_eq!(err, DocumentError::ReadOnly);
        assert_eq!(doc.source(), TINY_USDA);
        assert_eq!(doc.generation(), 0);
    }

    #[test]
    fn replace_source_round_trips_via_undo_redo() {
        let mut host = DocumentHost::new(UsdDocument::new(DocumentId::new(4), TINY_USDA));
        let new_text = "#usda 1.0\ndef Xform \"Other\" {}\n";
        host.apply(Mutation::local(UsdOp::ReplaceSource {
            edit_target: LayerId::root(),
            text: new_text.to_string(),
        }))
        .unwrap();
        assert_eq!(host.document().source(), new_text);
        assert_eq!(host.generation(), 1);

        host.undo().unwrap();
        assert_eq!(host.document().source(), TINY_USDA);
        // Generation is monotonic: undo bumps it too.
        assert_eq!(host.generation(), 2);

        host.redo().unwrap();
        assert_eq!(host.document().source(), new_text);
        assert_eq!(host.generation(), 3);
    }

    #[test]
    fn mark_saved_clears_dirty() {
        let mut doc = UsdDocument::new(DocumentId::new(5), TINY_USDA);
        assert!(doc.is_dirty());
        doc.mark_saved();
        assert!(!doc.is_dirty());
        let _ = doc.apply(UsdOp::ReplaceSource {
            edit_target: LayerId::root(),
            text: "changed".to_string(),
        });
        assert!(doc.is_dirty());
    }

    #[test]
    fn changes_since_returns_only_new_tail() {
        let mut doc = UsdDocument::new(DocumentId::new(6), TINY_USDA);
        let _ = doc.apply(UsdOp::ReplaceSource {
            edit_target: LayerId::root(),
            text: "a".to_string(),
        });
        let after_first = doc.generation();
        let _ = doc.apply(UsdOp::ReplaceSource {
            edit_target: LayerId::root(),
            text: "b".to_string(),
        });
        let tail: Vec<_> = doc.changes_since(after_first).collect();
        assert_eq!(tail.len(), 1);
        assert!(matches!(tail[0].1, UsdChange::FullReload));
    }

    #[test]
    fn non_root_edit_target_is_rejected_in_phase_1() {
        let mut doc = UsdDocument::new(DocumentId::new(7), TINY_USDA);
        let err = doc
            .apply(UsdOp::ReplaceSource {
                edit_target: LayerId::new("sub.usda"),
                text: "x".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, DocumentError::ValidationFailed(_)));
        assert_eq!(doc.generation(), 0);
    }
}
