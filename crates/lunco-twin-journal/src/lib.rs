//! # lunco-twin-journal
//!
//! Append-only, author-scoped journal of every change within a Twin.
//!
//! Records every applied document op, raw text edit, and lifecycle event into
//! a single canonical log scoped to a Twin. Per-author undo and per-document
//! / per-twin scopes are filtered views over the same log — so a user can
//! undo *their* edits without clobbering a peer's interleaved work.
//!
//! ## Architectural shape
//!
//! - **Entries** are immutable, identified by `(author, lamport)` pairs.
//!   Lamport clocks give causal ordering without wall-clock dependence and
//!   align with `yrs` CRDT IDs (`(client_id, clock)`) for future swap-in.
//! - **Streams** are named sequences of entries with a composition policy
//!   (Sequential / Layered / LastWriteWins). Branches and USD-style layers
//!   are both Streams under different policies.
//! - **JournalState** is the projected state computed by replaying entries
//!   from one-or-more streams under a Composition policy (lazy; foundation
//!   only implements Sequential).
//! - **ChangeSets** are optional atomic groups (transaction-style undo unit).
//! - **Markers** are user-named milestones in history (Onshape Versions, git
//!   tags, SysML v2 named Versions).
//! - **Branches** are mutable named refs to entries on a stream — never
//!   stored on entries themselves.
//!
//! Domains (Modelica, USD, SysML v2, Python, …) plug in by implementing
//! [`OpPayload`] for their op type. The journal is generic; domains know
//! nothing of the journal beyond emitting op + inverse pairs through a
//! [`JournalSink`].
//!
//! ## Today: in-memory, single Sequential stream
//!
//! The foundation supports one Twin, one `main` Sequential stream, single
//! user. The schema is shaped so multi-stream / multi-author / `Layered`
//! USD composition / yrs CRDT backend slot in without API changes.
//!
//! ## What this crate is NOT
//!
//! - **Not a runtime telemetry pipe** — telemetry stays on Bevy events.
//! - **Not the network transport** — `lunco-networking` will subscribe to
//!   entry-append events and broadcast.
//! - **Not the persistence layer** — entries live in memory today; backend
//!   swap (yrs / disk) replaces `Journal` internals only.

#![forbid(unsafe_code)]

use lunco_doc::DocumentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// ─────────────────────────────────────────────────────────────────────────────
// AuthorTag / AuthorId — identity
// ─────────────────────────────────────────────────────────────────────────────

/// Stable identity of an author for Lamport ordering and undo grouping.
///
/// Single string keyed off the user (not the tool) so the same human across
/// workbench + CLI + agent shares an undo stack. Tool metadata lives on
/// [`AuthorTag`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct AuthorId(pub String);

impl AuthorId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn local() -> Self {
        Self("local".to_string())
    }
}

impl From<&AuthorTag> for AuthorId {
    fn from(tag: &AuthorTag) -> Self {
        Self(tag.user.clone())
    }
}

/// Who/what authored an entry. `user` is the stable identity (matches
/// [`AuthorId`]); `tool` is metadata for telemetry, filtering, and UI.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct AuthorTag {
    pub user: String,
    pub tool: String,
}

impl AuthorTag {
    pub fn local_user() -> Self {
        Self {
            user: "local".into(),
            tool: "workbench".into(),
        }
    }

    pub fn for_tool(tool: impl Into<String>) -> Self {
        Self {
            user: "local".into(),
            tool: tool.into(),
        }
    }

    pub fn id(&self) -> AuthorId {
        AuthorId(self.user.clone())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Twin / Domain identity
// ─────────────────────────────────────────────────────────────────────────────

/// Identity of the Twin a journal belongs to. One Journal per Twin.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TwinId(pub String);

impl TwinId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// Discriminator for op payloads. Domains register themselves by name; the
/// journal uses this to dispatch decode + apply to the right handler.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DomainKind {
    Modelica,
    Usd,
    Sysml,
    Python,
    Other(String),
}

/// Typed reference to a domain entity. Used for conflict detection and
/// cross-domain link tracking. `path` is a stable, domain-defined identity
/// (e.g. `"MyClass.k"` for Modelica, `"/world/rover/wheel0"` for USD).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityRef {
    pub doc: DocumentId,
    pub domain: DomainKind,
    pub path: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// OpPayload trait — domains implement this
// ─────────────────────────────────────────────────────────────────────────────

/// Trait every domain op type implements to participate in the journal.
///
/// Required: serialize/deserialize, declare its domain. Optional but
/// recommended: declare which entities it touches, so the journal can
/// detect supersession (other authors editing the same entity) when
/// multi-user lands.
pub trait OpPayload: Serialize + Send + Sync + 'static {
    fn domain(&self) -> DomainKind;

    /// Entities this op touches. Default empty; domains override when they
    /// can answer the question. Used for conflict detection in collab mode.
    fn referenced_entities(&self) -> Vec<EntityRef> {
        Vec::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EntryId + Lamport clock
// ─────────────────────────────────────────────────────────────────────────────

/// Globally-unique id for a journal entry. Author + Lamport timestamp.
///
/// Two authors can never produce the same id; same author monotonically
/// increases lamport. This shape matches yrs `(client_id, clock)` so the
/// future CRDT swap is mechanical.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct EntryId {
    pub author: AuthorId,
    pub lamport: u64,
}

/// Logical clock for one author's local view. Observe remote ids on apply
/// to keep causality (`lamport = max(local, remote) + 1`).
#[derive(Debug, Default)]
pub struct LamportClock {
    value: AtomicU64,
}

impl LamportClock {
    pub fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }

    /// Observe an external lamport time and return current.
    pub fn observe(&self, remote: u64) -> u64 {
        let mut cur = self.value.load(Ordering::Acquire);
        loop {
            let next = cur.max(remote);
            match self.value.compare_exchange(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return next,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Allocate the next lamport for a local event.
    pub fn tick(&self) -> u64 {
        self.value.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn current(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JournalEntry + EntryKind
// ─────────────────────────────────────────────────────────────────────────────

/// One immutable recorded change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: EntryId,
    /// Causal predecessors in the DAG. Empty for genesis entries (e.g. the
    /// first `Snapshot` after import). Single-parent for linear edits.
    /// Multi-parent for merges.
    pub parents: Vec<EntryId>,
    pub author: AuthorTag,
    /// Wall-clock timestamp in ms since UNIX epoch (advisory; ordering uses
    /// lamport, not this).
    pub at_ms: u64,
    pub twin: TwinId,
    pub doc: DocumentId,
    pub kind: EntryKind,
    /// Optional grouping for atomic undo (transaction).
    pub change_set: Option<ChangeSetId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntryKind {
    /// Domain op was applied. Payload + inverse are domain-typed JSON.
    Op {
        domain: DomainKind,
        op: serde_json::Value,
        inverse: serde_json::Value,
    },
    /// Raw byte-range text edit (code-pane keystrokes after debounce, or
    /// any non-structural edit). Inverse is a `TextEdit` with swapped
    /// range/replacement.
    TextEdit {
        range: std::ops::Range<usize>,
        replacement: String,
        inverse_range: std::ops::Range<usize>,
        inverse_replacement: String,
    },
    /// Initial state of a document — a full source snapshot. Used at
    /// import/load time; ops then build on top.
    Snapshot {
        domain: DomainKind,
        source: String,
    },
    /// Lifecycle event (no state mutation; informational).
    Lifecycle(LifecycleKind),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LifecycleKind {
    Opened { source_hash: String },
    Saved,
    Closed,
}

// ─────────────────────────────────────────────────────────────────────────────
// ChangeSet — optional atomic group of entries
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ChangeSetId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeSet {
    pub id: ChangeSetId,
    pub label: String,
    pub author: AuthorTag,
    pub at_ms: u64,
    pub entries: Vec<EntryId>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Marker — named milestone (Onshape Version / git tag / SysML v2 Version)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct MarkerId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Marker {
    pub id: MarkerId,
    pub name: String,
    pub message: String,
    pub head: EntryId,
    pub author: AuthorTag,
    pub at_ms: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Stream + Composition
// ─────────────────────────────────────────────────────────────────────────────

/// Stream identity. Streams are named sequences of entries; branches and
/// USD-style layers are both Streams under different composition policies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StreamId(pub String);

impl StreamId {
    pub fn main() -> Self {
        Self("main".to_string())
    }
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

/// How multiple streams combine into a [`JournalState`].
///
/// Foundation only implements `Sequential`; `Layered` and `LastWriteWins`
/// are typed so future work doesn't require schema migration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Composition {
    /// Single-active stream (git, SysML v2 commits, Onshape workspaces).
    /// Switching streams = changing which one we read; merging requires an
    /// explicit op producing a multi-parent entry.
    Sequential,
    /// Multiple-active streams compose by layer rules (USD layer stack,
    /// Nucleus per-user layers). Same-attribute conflicts resolved by
    /// layer-strength ordering.
    Layered { rules: LayerRules },
    /// All streams apply continuously, latest lamport wins. Real-time
    /// collab on flat content (Modelica/Python without a layer model).
    LastWriteWins,
}

/// Placeholder for future USD-style layer composition rules. Foundation
/// leaves this empty; populated when `Composition::Layered` is implemented.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LayerRules {
    /// Reserved. Stronger streams override weaker on same-entity conflicts.
    pub strength_order: Vec<StreamId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub id: StreamId,
    pub name: String,
    pub composition: Composition,
    /// Latest entry on this stream. `None` for empty stream.
    pub head: Option<EntryId>,
    /// Streams this one was forked from (for branch ancestry). Empty for
    /// genesis streams like `main`.
    pub parent_streams: Vec<StreamId>,
    pub created_at_ms: u64,
    pub created_by: AuthorTag,
}

// ─────────────────────────────────────────────────────────────────────────────
// Branch — mutable named ref
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    pub stream: StreamId,
    /// Mutable: advances on append + fast-forward + merge.
    pub head: EntryId,
}

// ─────────────────────────────────────────────────────────────────────────────
// JournalState — projected state from one-or-more streams
// ─────────────────────────────────────────────────────────────────────────────

/// Computed state of a Twin at a point in time, derived by replaying
/// entries from one-or-more streams under a [`Composition`] policy.
///
/// Foundation: only `Composition::Sequential` is computable. `Layered` and
/// `LastWriteWins` are typed-but-`unimplemented!()` until USD live collab
/// and real-time-collab features arrive.
///
/// State is computed lazily — the type holds the recipe (which streams,
/// which composition), not the materialized result. Domain consumers
/// re-derive their projections (Modelica `Index`, USD composed scene, etc.)
/// from the journal entries selected here.
#[derive(Debug, Clone)]
pub struct JournalState {
    pub streams: Vec<StreamId>,
    pub composition: Composition,
    /// Entry id at which this state was projected. Anchors the projection
    /// against the journal's append history.
    pub head: Option<EntryId>,
}

impl JournalState {
    pub fn new(streams: Vec<StreamId>, composition: Composition) -> Self {
        Self {
            streams,
            composition,
            head: None,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JournalSink — write-side interface
// ─────────────────────────────────────────────────────────────────────────────

/// Where journal entries are recorded. `DocumentHost::apply` and other
/// mutation paths take an `&dyn JournalSink` so they emit entries without
/// directly depending on the journal type. Bevy installs a sink that
/// points at a shared `JournalResource`; CLI / headless tests can install
/// [`NullSink`] to skip recording.
pub trait JournalSink: Send + Sync {
    fn record(&self, entry: JournalEntry);
}

/// No-op sink. Useful in headless tests that don't need history.
pub struct NullSink;

impl JournalSink for NullSink {
    fn record(&self, _entry: JournalEntry) {}
}

// ─────────────────────────────────────────────────────────────────────────────
// Journal — the canonical store
// ─────────────────────────────────────────────────────────────────────────────

/// Canonical record of every change in a Twin.
///
/// Today: in-memory `HashMap<EntryId, JournalEntry>` + ordered insertion
/// log. Tomorrow: yrs::Doc backend swap; the public API in this crate is
/// shaped so that swap is internal and callers don't change.
///
/// A Journal owns: entries (immutable once written), streams, branches,
/// change sets, markers. Single Twin scope.
pub struct Journal {
    twin: TwinId,
    local_author: AuthorId,
    clock: LamportClock,

    // TODO(crdt): replace these maps with yrs::Doc structures.
    entries: HashMap<EntryId, JournalEntry>,
    /// Total order of insertion. Used for replay and iteration; causal
    /// order via parent links is the source of truth for correctness.
    entry_order: Vec<EntryId>,

    streams: HashMap<StreamId, Stream>,
    branches: HashMap<String, Branch>,
    change_sets: HashMap<ChangeSetId, ChangeSet>,
    markers: HashMap<MarkerId, Marker>,

    next_change_set: u64,
    next_marker: u64,
}

impl Journal {
    /// Create a new Journal scoped to one Twin, with a default `main`
    /// stream and `main` branch. The local author identity is used to
    /// allocate `EntryId`s for locally-applied entries.
    pub fn new(twin: TwinId, local_author: AuthorId) -> Self {
        let mut j = Self {
            twin,
            local_author: local_author.clone(),
            clock: LamportClock::new(),
            entries: HashMap::new(),
            entry_order: Vec::new(),
            streams: HashMap::new(),
            branches: HashMap::new(),
            change_sets: HashMap::new(),
            markers: HashMap::new(),
            next_change_set: 0,
            next_marker: 0,
        };
        let main_stream = Stream {
            id: StreamId::main(),
            name: "main".to_string(),
            composition: Composition::Sequential,
            head: None,
            parent_streams: Vec::new(),
            created_at_ms: now_ms(),
            created_by: AuthorTag {
                user: local_author.0.clone(),
                tool: "system".to_string(),
            },
        };
        j.streams.insert(StreamId::main(), main_stream);
        j
    }

    pub fn twin(&self) -> &TwinId {
        &self.twin
    }

    pub fn local_author(&self) -> &AuthorId {
        &self.local_author
    }

    // ── Entry append ─────────────────────────────────────────────────────

    /// Append a new local entry. Allocates a fresh lamport for the local
    /// author, links to the current branch head as parent if present,
    /// advances the branch head.
    pub fn append_local(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        kind: EntryKind,
        change_set: Option<ChangeSetId>,
    ) -> EntryId {
        let lamport = self.clock.tick();
        let id = EntryId {
            author: self.local_author.clone(),
            lamport,
        };
        let parents = self
            .branches
            .get("main")
            .map(|b| vec![b.head.clone()])
            .unwrap_or_default();
        let entry = JournalEntry {
            id: id.clone(),
            parents,
            author,
            at_ms: now_ms(),
            twin: self.twin.clone(),
            doc,
            kind,
            change_set,
        };
        self.insert_entry(entry);
        self.advance_main(id.clone());
        if let Some(cs_id) = change_set {
            if let Some(cs) = self.change_sets.get_mut(&cs_id) {
                cs.entries.push(id.clone());
            }
        }
        id
    }

    /// Convenience: record a typed op and its inverse.
    pub fn record_op<O: OpPayload, I: OpPayload>(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        op: &O,
        inverse: &I,
        change_set: Option<ChangeSetId>,
    ) -> Result<EntryId, serde_json::Error> {
        let kind = EntryKind::Op {
            domain: op.domain(),
            op: serde_json::to_value(op)?,
            inverse: serde_json::to_value(inverse)?,
        };
        Ok(self.append_local(author, doc, kind, change_set))
    }

    /// Record an op whose payload is already a `serde_json::Value`.
    ///
    /// Used by domains that haven't yet derived `Serialize` on their op
    /// type — they build a structured summary by hand and record it.
    /// Equivalent to [`record_op`] minus the typed serialize step.
    pub fn record_op_value(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        domain: DomainKind,
        op: serde_json::Value,
        inverse: serde_json::Value,
        change_set: Option<ChangeSetId>,
    ) -> EntryId {
        let kind = EntryKind::Op { domain, op, inverse };
        self.append_local(author, doc, kind, change_set)
    }

    /// Convenience: record a raw byte-range text edit.
    pub fn record_text_edit(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        range: std::ops::Range<usize>,
        replacement: String,
        inverse_range: std::ops::Range<usize>,
        inverse_replacement: String,
        change_set: Option<ChangeSetId>,
    ) -> EntryId {
        let kind = EntryKind::TextEdit {
            range,
            replacement,
            inverse_range,
            inverse_replacement,
        };
        self.append_local(author, doc, kind, change_set)
    }

    /// Convenience: record a lifecycle event.
    pub fn record_lifecycle(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        kind: LifecycleKind,
    ) -> EntryId {
        self.append_local(author, doc, EntryKind::Lifecycle(kind), None)
    }

    /// Convenience: record an initial source snapshot for a document.
    pub fn record_snapshot(
        &mut self,
        author: AuthorTag,
        doc: DocumentId,
        domain: DomainKind,
        source: String,
    ) -> EntryId {
        self.append_local(author, doc, EntryKind::Snapshot { domain, source }, None)
    }

    /// Insert an entry produced elsewhere (a remote peer, journal replay,
    /// loaded session). Observes the entry's lamport into the local clock
    /// to keep causality, then inserts. If the entry's id is already
    /// known, it's a no-op (idempotent replay).
    ///
    /// `main` branch advancement: if the new entry's parents include the
    /// current main head, fast-forward main to the new entry. Otherwise
    /// the entry lives in the DAG but main stays put — the caller (or a
    /// future merge resolver) decides what to do.
    pub fn append_remote(&mut self, entry: JournalEntry) {
        if self.entries.contains_key(&entry.id) {
            return; // idempotent
        }
        self.clock.observe(entry.id.lamport);

        let id = entry.id.clone();
        let parents = entry.parents.clone();
        let change_set = entry.change_set;

        self.insert_entry(entry);

        // Fast-forward main if the remote entry extends our current head.
        let main_head = self
            .branches
            .get("main")
            .map(|b| b.head.clone())
            .or_else(|| self.streams.get(&StreamId::main()).and_then(|s| s.head.clone()));
        let extends_main = match &main_head {
            Some(h) => parents.contains(h),
            None => parents.is_empty(),
        };
        if extends_main {
            self.advance_main(id.clone());
        }

        if let Some(cs_id) = change_set {
            if let Some(cs) = self.change_sets.get_mut(&cs_id) {
                cs.entries.push(id);
            }
        }
    }

    fn insert_entry(&mut self, entry: JournalEntry) {
        self.entry_order.push(entry.id.clone());
        self.entries.insert(entry.id.clone(), entry);
    }

    fn advance_main(&mut self, head: EntryId) {
        let stream = self.streams.get_mut(&StreamId::main()).expect("main stream exists");
        stream.head = Some(head.clone());
        self.branches
            .entry("main".to_string())
            .and_modify(|b| b.head = head.clone())
            .or_insert(Branch {
                name: "main".to_string(),
                stream: StreamId::main(),
                head,
            });
    }

    // ── Reads ────────────────────────────────────────────────────────────

    pub fn get(&self, id: &EntryId) -> Option<&JournalEntry> {
        self.entries.get(id)
    }

    pub fn entries(&self) -> impl Iterator<Item = &JournalEntry> {
        self.entry_order.iter().filter_map(|id| self.entries.get(id))
    }

    pub fn entries_for_doc(&self, doc: DocumentId) -> impl Iterator<Item = &JournalEntry> {
        self.entries().filter(move |e| e.doc == doc)
    }

    pub fn entries_by_author<'a>(
        &'a self,
        author: &'a AuthorId,
    ) -> impl Iterator<Item = &'a JournalEntry> + 'a {
        self.entries().filter(move |e| &e.id.author == author)
    }

    pub fn len(&self) -> usize {
        self.entry_order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entry_order.is_empty()
    }

    // ── Streams ──────────────────────────────────────────────────────────

    pub fn stream(&self, id: &StreamId) -> Option<&Stream> {
        self.streams.get(id)
    }

    pub fn create_stream(
        &mut self,
        id: StreamId,
        name: String,
        composition: Composition,
        parent_streams: Vec<StreamId>,
        created_by: AuthorTag,
    ) -> &Stream {
        let stream = Stream {
            id: id.clone(),
            name,
            composition,
            head: None,
            parent_streams,
            created_at_ms: now_ms(),
            created_by,
        };
        self.streams.insert(id.clone(), stream);
        self.streams.get(&id).expect("just inserted")
    }

    // ── Branches ─────────────────────────────────────────────────────────

    pub fn branch(&self, name: &str) -> Option<&Branch> {
        self.branches.get(name)
    }

    pub fn branches(&self) -> impl Iterator<Item = &Branch> {
        self.branches.values()
    }

    // ── ChangeSets ───────────────────────────────────────────────────────

    pub fn open_change_set(&mut self, label: String, author: AuthorTag) -> ChangeSetId {
        let id = ChangeSetId(self.next_change_set);
        self.next_change_set = self.next_change_set.saturating_add(1);
        self.change_sets.insert(
            id,
            ChangeSet {
                id,
                label,
                author,
                at_ms: now_ms(),
                entries: Vec::new(),
            },
        );
        id
    }

    pub fn change_set(&self, id: ChangeSetId) -> Option<&ChangeSet> {
        self.change_sets.get(&id)
    }

    // ── Markers (named versions) ─────────────────────────────────────────

    pub fn create_marker(
        &mut self,
        name: String,
        message: String,
        head: EntryId,
        author: AuthorTag,
    ) -> MarkerId {
        let id = MarkerId(self.next_marker);
        self.next_marker = self.next_marker.saturating_add(1);
        self.markers.insert(
            id,
            Marker {
                id,
                name,
                message,
                head,
                author,
                at_ms: now_ms(),
            },
        );
        id
    }

    pub fn marker(&self, id: MarkerId) -> Option<&Marker> {
        self.markers.get(&id)
    }

    pub fn markers(&self) -> impl Iterator<Item = &Marker> {
        self.markers.values()
    }

    // ── Projection ───────────────────────────────────────────────────────

    /// Build a [`JournalState`] over the `main` stream with `Sequential`
    /// composition. This is the foundation default; richer projections
    /// (multi-stream, Layered) come later.
    pub fn project_main(&self) -> JournalState {
        let head = self
            .streams
            .get(&StreamId::main())
            .and_then(|s| s.head.clone());
        JournalState {
            streams: vec![StreamId::main()],
            composition: Composition::Sequential,
            head,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UndoManager — per-author intent stack with scope filter
// ─────────────────────────────────────────────────────────────────────────────

/// Scope of an undo operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoScope {
    /// Undo within a specific document only.
    Document(DocumentId),
    /// Undo any of this author's edits anywhere in the Twin (Workspace
    /// Undo / Ctrl-Shift-Z).
    Twin,
}

/// Per-author undo manager. Owns two stacks of [`EntryId`].
///
/// `record_local(id)` is called whenever this author appends an op-entry.
/// `take_undo(scope, journal)` pops the most recent matching id; the
/// workbench then dispatches the *inverse* op as a fresh op (which itself
/// becomes another journal entry, recorded back via [`record_redo`] so
/// the next undo reverses it again).
pub struct UndoManager {
    pub author: AuthorTag,
    undo_stack: Vec<EntryId>,
    redo_stack: Vec<EntryId>,
}

impl UndoManager {
    pub fn new(author: AuthorTag) -> Self {
        Self {
            author,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn record_local(&mut self, entry_id: EntryId) {
        self.undo_stack.push(entry_id);
        self.redo_stack.clear();
    }

    pub fn record_redo(&mut self, entry_id: EntryId) {
        self.undo_stack.push(entry_id);
    }

    /// Take the most recent undoable entry within a scope.
    pub fn take_undo(&mut self, scope: &UndoScope, journal: &Journal) -> Option<EntryId> {
        let pos = self.undo_stack.iter().rposition(|id| {
            journal
                .get(id)
                .map(|e| matches_scope(e, scope))
                .unwrap_or(false)
        })?;
        let id = self.undo_stack.remove(pos);
        self.redo_stack.push(id.clone());
        Some(id)
    }

    pub fn take_redo(&mut self, scope: &UndoScope, journal: &Journal) -> Option<EntryId> {
        let pos = self.redo_stack.iter().rposition(|id| {
            journal
                .get(id)
                .map(|e| matches_scope(e, scope))
                .unwrap_or(false)
        })?;
        Some(self.redo_stack.remove(pos))
    }

    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    /// Drop all entries referencing `doc` (used when a doc is closed without
    /// saving so undo doesn't resurrect deleted documents).
    pub fn drop_doc(&mut self, doc: DocumentId, journal: &Journal) {
        self.undo_stack.retain(|id| {
            journal.get(id).map(|e| e.doc != doc).unwrap_or(true)
        });
        self.redo_stack.retain(|id| {
            journal.get(id).map(|e| e.doc != doc).unwrap_or(true)
        });
    }
}

fn matches_scope(entry: &JournalEntry, scope: &UndoScope) -> bool {
    match scope {
        UndoScope::Document(d) => &entry.doc == d,
        UndoScope::Twin => true,
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
    use serde::{Deserialize, Serialize};

    // A fake Modelica-flavored op for testing the OpPayload trait.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FakeOp {
        class: String,
        name: String,
        value: f64,
    }

    impl OpPayload for FakeOp {
        fn domain(&self) -> DomainKind {
            DomainKind::Modelica
        }
        fn referenced_entities(&self) -> Vec<EntityRef> {
            vec![EntityRef {
                doc: DocumentId::new(1),
                domain: DomainKind::Modelica,
                path: format!("{}.{}", self.class, self.name),
            }]
        }
    }

    fn new_journal() -> Journal {
        Journal::new(TwinId::new("test-twin"), AuthorId::local())
    }

    #[test]
    fn fresh_journal_has_main_stream_and_no_entries() {
        let j = new_journal();
        assert!(j.is_empty());
        assert_eq!(j.len(), 0);
        assert!(j.stream(&StreamId::main()).is_some());
        assert!(j.branch("main").is_none()); // branch only created on first append
    }

    #[test]
    fn append_local_advances_main_branch_and_clock() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let inv = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 1.0,
        };
        let id = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &inv, None)
            .unwrap();
        assert_eq!(id.author, AuthorId::local());
        assert_eq!(id.lamport, 1);
        assert_eq!(j.len(), 1);
        let head = j.branch("main").map(|b| b.head.clone());
        assert_eq!(head.as_ref(), Some(&id));
    }

    #[test]
    fn parents_link_to_previous_main_head() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let id_a = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        let id_b = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        let entry_b = j.get(&id_b).unwrap();
        assert_eq!(entry_b.parents, vec![id_a]);
    }

    #[test]
    fn change_set_groups_entries() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let cs = j.open_change_set("Rename Foo→Bar".into(), AuthorTag::local_user());
        j.record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, Some(cs))
            .unwrap();
        j.record_op(AuthorTag::local_user(), DocumentId::new(2), &op, &op, Some(cs))
            .unwrap();
        let cs_ref = j.change_set(cs).unwrap();
        assert_eq!(cs_ref.entries.len(), 2);
        assert_eq!(cs_ref.label, "Rename Foo→Bar");
    }

    #[test]
    fn marker_anchors_a_named_version() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let id = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        let marker = j.create_marker(
            "v1.0".into(),
            "First milestone".into(),
            id.clone(),
            AuthorTag::local_user(),
        );
        let m = j.marker(marker).unwrap();
        assert_eq!(m.name, "v1.0");
        assert_eq!(m.head, id);
    }

    #[test]
    fn undo_per_doc_skips_other_docs() {
        let mut j = new_journal();
        let mut um = UndoManager::new(AuthorTag::local_user());
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let a = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        um.record_local(a.clone());
        let b = j
            .record_op(AuthorTag::local_user(), DocumentId::new(2), &op, &op, None)
            .unwrap();
        um.record_local(b);

        let undone = um.take_undo(&UndoScope::Document(DocumentId::new(1)), &j);
        assert_eq!(undone, Some(a));
    }

    #[test]
    fn undo_twin_scope_takes_latest() {
        let mut j = new_journal();
        let mut um = UndoManager::new(AuthorTag::local_user());
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let _a = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        um.record_local(_a);
        let b = j
            .record_op(AuthorTag::local_user(), DocumentId::new(2), &op, &op, None)
            .unwrap();
        um.record_local(b.clone());

        let undone = um.take_undo(&UndoScope::Twin, &j);
        assert_eq!(undone, Some(b));
    }

    #[test]
    fn lamport_clock_observes_remote_times() {
        let clk = LamportClock::new();
        clk.tick(); // 1
        clk.tick(); // 2
        let observed = clk.observe(10);
        assert_eq!(observed, 10);
        let next = clk.tick();
        assert_eq!(next, 11);
    }

    #[test]
    fn null_sink_is_a_no_op() {
        let sink = NullSink;
        let entry = JournalEntry {
            id: EntryId {
                author: AuthorId::local(),
                lamport: 1,
            },
            parents: Vec::new(),
            author: AuthorTag::local_user(),
            at_ms: 0,
            twin: TwinId::new("t"),
            doc: DocumentId::new(1),
            kind: EntryKind::Lifecycle(LifecycleKind::Saved),
            change_set: None,
        };
        sink.record(entry); // doesn't panic
    }

    #[test]
    fn projection_targets_main_stream() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let id = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        let state = j.project_main();
        assert_eq!(state.streams, vec![StreamId::main()]);
        assert!(matches!(state.composition, Composition::Sequential));
        assert_eq!(state.head, Some(id));
    }

    #[test]
    fn op_payload_reports_referenced_entities() {
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let refs = op.referenced_entities();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "Foo.k");
        assert_eq!(refs[0].domain, DomainKind::Modelica);
    }

    #[test]
    fn append_remote_observes_clock_and_extends_main() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        let local_id = j
            .record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();

        // Forge a remote entry that builds on local_id.
        let remote_id = EntryId {
            author: AuthorId::new("peer"),
            lamport: 42,
        };
        let remote = JournalEntry {
            id: remote_id.clone(),
            parents: vec![local_id],
            author: AuthorTag {
                user: "peer".into(),
                tool: "remote".into(),
            },
            at_ms: 0,
            twin: TwinId::new("test-twin"),
            doc: DocumentId::new(1),
            kind: EntryKind::Lifecycle(LifecycleKind::Saved),
            change_set: None,
        };
        j.append_remote(remote);

        // Local clock observed the remote lamport.
        assert!(j.clock.current() >= 42);
        // main fast-forwarded to the remote entry.
        assert_eq!(j.branch("main").map(|b| b.head.clone()), Some(remote_id));
        // Idempotent re-apply is a no-op.
        let len_before = j.len();
        let dup = JournalEntry {
            id: EntryId {
                author: AuthorId::new("peer"),
                lamport: 42,
            },
            parents: Vec::new(),
            author: AuthorTag::default(),
            at_ms: 0,
            twin: TwinId::new("test-twin"),
            doc: DocumentId::new(1),
            kind: EntryKind::Lifecycle(LifecycleKind::Saved),
            change_set: None,
        };
        j.append_remote(dup);
        assert_eq!(j.len(), len_before);
    }

    #[test]
    fn entries_by_author_filter() {
        let mut j = new_journal();
        let op = FakeOp {
            class: "Foo".into(),
            name: "k".into(),
            value: 2.0,
        };
        j.record_op(AuthorTag::local_user(), DocumentId::new(1), &op, &op, None)
            .unwrap();
        let count = j.entries_by_author(&AuthorId::local()).count();
        assert_eq!(count, 1);
        let other = AuthorId::new("nobody");
        assert_eq!(j.entries_by_author(&other).count(), 0);
    }
}
