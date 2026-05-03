//! # lunco-twin-journal
//!
//! Append-only, author-scoped op log with per-user undo.
//!
//! Records every applied document op (and lifecycle event) into a single
//! journal scoped to a Twin. Per-author undo stacks are filtered views over
//! the same log — so a user can undo *their* edits without clobbering a
//! peer's interleaved work, and per-document undo is a further filter on
//! top of per-author.
//!
//! ## Today: local Vec-backed
//!
//! For single-user / WASM the journal is just an ordered `Vec<JournalEntry>`
//! — minimal overhead, smallest possible bundle.
//!
//! ## Tomorrow: CRDT-backed (yrs)
//!
//! When we add multi-user sync, the internal log becomes a `yrs::Doc`
//! `Array` — author-scoped undo and structural-op CRDT semantics come
//! along for free. The API in this crate is shaped so callers don't notice
//! the swap; only `Journal`'s internals change. See `TODO(crdt)` markers.
//!
//! ## What this crate is NOT
//!
//! - **Not a runtime telemetry pipe** — telemetry stays on Bevy events
//!   for now. If we eventually unify "edit ops" and "observations" into one
//!   journal envelope, this is the crate that grows; for now we keep the
//!   schema focused on edits + lifecycle.
//! - **Not the network transport** — when sync arrives, transport lives in
//!   `lunco-networking`; this crate exposes encode/decode hooks.

#![forbid(unsafe_code)]

use lunco_doc::DocumentId;
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// AuthorTag — who issued the op
// ─────────────────────────────────────────────────────────────────────────────

/// Who/what authored a journal entry. Single-user has one author; multi-user
/// and AI-agent flows distinguish peers via this tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct AuthorTag {
    /// Stable user identity (username, email, or anonymous uuid).
    pub user: String,
    /// Originating tool (e.g. `"workbench"`, `"cli"`, `"agent:claude"`).
    pub tool: String,
}

impl AuthorTag {
    /// Convenience: local single-user default.
    pub fn local_user() -> Self {
        Self {
            user: "local".to_string(),
            tool: "workbench".to_string(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JournalEntry
// ─────────────────────────────────────────────────────────────────────────────

/// A single recorded event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Monotonic per-Journal id. Stable across this Journal's lifetime.
    pub id: u64,
    /// Wall-clock timestamp in ms since UNIX epoch.
    pub timestamp_ms: u64,
    pub author: AuthorTag,
    pub doc: DocumentId,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntryKind {
    /// A mutation was applied. `op` is the original; `inverse` reverts it.
    /// Both are domain-specific JSON values (engines define their op types).
    OpApplied {
        op: serde_json::Value,
        inverse: serde_json::Value,
    },
    DocOpened {
        source_hash: String,
    },
    DocClosed,
    DocSaved,
}

// ─────────────────────────────────────────────────────────────────────────────
// Journal — the append-only log
// ─────────────────────────────────────────────────────────────────────────────

/// Append-only journal of typed entries.
///
/// Today: simple `Vec`. Tomorrow: yrs-backed `Array` for CRDT semantics.
/// The public surface is the same in both shapes.
pub struct Journal {
    // TODO(crdt): replace with `yrs::Doc` holding an `Array` of entries.
    entries: Vec<JournalEntry>,
    next_id: u64,
}

impl Default for Journal {
    fn default() -> Self {
        Self::new()
    }
}

impl Journal {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 0,
        }
    }

    /// Append a new entry, return its id.
    pub fn append(&mut self, author: AuthorTag, doc: DocumentId, kind: EntryKind) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(JournalEntry {
            id,
            timestamp_ms: now_ms(),
            author,
            doc,
            kind,
        });
        id
    }

    /// Convenience: record an op-applied entry. `op` and `inverse` should
    /// be the engine's serialized op forms.
    pub fn record_op(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        op: serde_json::Value,
        inverse: serde_json::Value,
    ) -> u64 {
        self.append(author, doc, EntryKind::OpApplied { op, inverse })
    }

    /// All entries, in order.
    pub fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }

    /// Look up one entry by id (O(n) today; O(log n) once yrs-backed).
    pub fn get(&self, id: u64) -> Option<&JournalEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Length of the log.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UndoManager — per-author intent stack
// ─────────────────────────────────────────────────────────────────────────────

/// Per-author undo manager. Owns two stacks of [`JournalEntry`] ids.
///
/// `record_local(id)` is called whenever this author appends an op-entry.
/// `take_undo()` pops the most recent and returns it; the workbench then
/// dispatches the *inverse* op as a fresh op (which itself becomes another
/// journal entry, but is excluded from the undo stack via
/// [`UndoManager::record_redo`]).
///
/// Per-document undo is implemented externally by filtering returned entries
/// to a target `DocumentId` — the manager doesn't bake a per-doc split in,
/// so a single keypress can reach across docs when the focused-doc filter
/// is empty.
pub struct UndoManager {
    pub author: AuthorTag,
    undo_stack: Vec<u64>,
    redo_stack: Vec<u64>,
}

impl UndoManager {
    pub fn new(author: AuthorTag) -> Self {
        Self {
            author,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    /// Record a freshly-appended local op-entry id.
    pub fn record_local(&mut self, entry_id: u64) {
        self.undo_stack.push(entry_id);
        self.redo_stack.clear();
    }

    /// Record an entry id that resulted from `redo()` (so the next undo
    /// reverses it again).
    pub fn record_redo(&mut self, entry_id: u64) {
        self.undo_stack.push(entry_id);
    }

    /// Pop the most recent undoable entry id.
    /// Returns `None` if the stack is empty or filtered out.
    pub fn take_undo(&mut self, filter: impl Fn(u64) -> bool) -> Option<u64> {
        let pos = self.undo_stack.iter().rposition(|id| filter(*id))?;
        let id = self.undo_stack.remove(pos);
        self.redo_stack.push(id);
        Some(id)
    }

    /// Pop the most recent redoable entry id.
    pub fn take_redo(&mut self, filter: impl Fn(u64) -> bool) -> Option<u64> {
        let pos = self.redo_stack.iter().rposition(|id| filter(*id))?;
        let id = self.redo_stack.remove(pos);
        Some(id)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Drop all entries referencing `doc` (used when a doc is closed without
    /// saving to keep undo from resurrecting deleted documents).
    pub fn drop_doc(&mut self, predicate: impl Fn(u64) -> bool) {
        self.undo_stack.retain(|id| !predicate(*id));
        self.redo_stack.retain(|id| !predicate(*id));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Time
// ─────────────────────────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn append_and_read() {
        let mut j = Journal::new();
        let id = j.record_op(AuthorTag::local_user(), DocumentId::new(1), json!({"k":1}), json!({"k":-1}));
        assert_eq!(id, 0);
        assert_eq!(j.len(), 1);
        assert!(matches!(j.get(0).unwrap().kind, EntryKind::OpApplied { .. }));
    }

    #[test]
    fn undo_redo_roundtrip() {
        let mut j = Journal::new();
        let mut um = UndoManager::new(AuthorTag::local_user());
        let id_a = j.record_op(AuthorTag::local_user(), DocumentId::new(1), json!({}), json!({}));
        um.record_local(id_a);
        let id_b = j.record_op(AuthorTag::local_user(), DocumentId::new(1), json!({}), json!({}));
        um.record_local(id_b);

        let undone = um.take_undo(|_| true).unwrap();
        assert_eq!(undone, id_b);
        let redone = um.take_redo(|_| true).unwrap();
        assert_eq!(redone, id_b);
    }

    #[test]
    fn undo_filter_excludes_other_docs() {
        let mut j = Journal::new();
        let mut um = UndoManager::new(AuthorTag::local_user());
        let id_a = j.record_op(AuthorTag::local_user(), DocumentId::new(1), json!({}), json!({}));
        um.record_local(id_a);
        let id_b = j.record_op(AuthorTag::local_user(), DocumentId::new(2), json!({}), json!({}));
        um.record_local(id_b);

        // Per-doc undo on doc 1: should skip the doc-2 entry on top of stack.
        let undone = um
            .take_undo(|id| j.get(id).map(|e| e.doc == DocumentId::new(1)).unwrap_or(false))
            .unwrap();
        assert_eq!(undone, id_a);
    }
}
