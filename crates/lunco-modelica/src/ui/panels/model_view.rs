//! `ModelViewPanel` — multi-instance center tab, one per open document.
//!
//! Implements [`InstancePanel`] so the workbench can host arbitrarily
//! many model tabs in the center dock. Each tab's instance id is the
//! raw [`DocumentId`] it views; per-tab state (current view mode,
//! future: text cursor, pan/zoom) lives in the [`ModelTabs`] resource.
//!
//! Rendering strategy: every reader names the doc/tab it's reading.
//! Source + metadata derive from
//! `ModelicaDocumentRegistry::host(doc).document()`; per-tab UI
//! state (drilled scope, view mode, pinned) lives on
//! `ModelTabState`; per-doc compile state on `CompileStates`.
//! `EditorBufferState.bound_doc` is the typed identity for the
//! editor's current contents.

use std::collections::HashMap;

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_doc::DocumentId;
use lunco_workbench::{InstancePanel, Panel, PanelId, PanelSlot};

use crate::ui::panels::code_editor::EditorBufferState;
use crate::ui::panels::{
    canvas_diagram::CanvasDiagramPanel, code_editor::CodeEditorPanel,
};
use crate::ui::{CompileState, CompileStates, ModelicaDocumentRegistry, WorkbenchState};

/// The `PanelId` under which `ModelViewPanel` is registered as an
/// instance-panel kind. Instance ids are [`DocumentId::raw`] values.
pub const MODEL_VIEW_KIND: PanelId = PanelId("modelica_model_view");

/// Which rendering mode a model tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelViewMode {
    /// Raw Modelica source (egui TextEdit).
    Text,
    /// Block-diagram canvas, rendered on `lunco-canvas`.
    /// Default for new tabs — composed-model examples (the
    /// majority of MSL bundle and LunCo bundle) make sense visually
    /// before they make sense as text. Users who want code first
    /// flip the toolbar toggle.
    #[default]
    Canvas,
    /// The class's own `Icon` annotation rendering — what the
    /// component looks like when instantiated in a parent diagram.
    /// OMEdit/Dymola have Icon + Diagram as sibling views.
    Icon,
    /// The class's `Documentation(info="…", revisions="…")`
    /// annotation rendered as text. HTML is shown as-is (no
    /// Markdown conversion yet) — reads like rendered plain text
    /// with tags visible, which is honest about what's in source
    /// and avoids guessing at formatting.
    Docs,
}

/// Per-tab state for a [`ModelViewPanel`] instance.
///
/// Tabs are now keyed by an opaque [`TabId`] (a counter-allocated
/// `u64`) rather than the [`DocumentId`] they view. This lets the
/// same document live in multiple tabs (e.g. a Text view and a
/// Canvas view side-by-side) and lets sibling classes from the same
/// `.mo` file open in distinct tabs (drilled-in classes set
/// `drilled_class`).
#[derive(Debug, Clone)]
pub struct ModelTabState {
    /// The Document this tab is viewing.
    pub doc: DocumentId,
    /// Qualified class name this tab is scoped to, when the tab was
    /// opened via drill-in. `None` for plain "open document" tabs;
    /// `Some("Modelica.Blocks.Continuous.PID")` for drilled-in tabs.
    /// Distinct values produce distinct tabs even when `doc` matches.
    pub drilled_class: Option<String>,
    /// Text vs Diagram vs Icon vs Docs.
    pub view_mode: ModelViewMode,
    /// VS Code-style preview tab semantics. An *unpinned* tab is the
    /// "preview" — clicking another class in the package browser
    /// repurposes it (changes its `doc` / `drilled_class`) instead
    /// of opening a third tab, so casual browse-around clicks don't
    /// pile up dozens of tabs. The first edit, drill-in, or explicit
    /// "Pin" action sets `pinned = true` and the tab becomes a
    /// permanent fixture; the next browser click then opens a fresh
    /// preview alongside it.
    pub pinned: bool,
    /// Set when an off-thread drill-in / duplicate load fails (e.g.
    /// MSL bundle not yet available, class missing, parse error).
    /// Read by the canvas to swap the spinner for an error card so
    /// the tab doesn't sit on "Loading resource…" forever.
    pub load_error: Option<String>,
}

/// Newtype-ish alias for tab instance ids. Stored on the workbench
/// dock layer as a `u64`; we just give it a name here so call-site
/// intent is readable.
pub type TabId = u64;

/// Set by [`ModelViewPanel::render`] for the duration of a body
/// (canvas / code editor / icon / docs) render call so the body can
/// scope its work to *this* tab without consulting any singleton.
///
/// Bodies prefer this resource over `WorkspaceResource.active_document`
/// — that singleton tracks "which tab has focus" workspace-wide; on a
/// split, both panes render but only one has focus, so reading the
/// singleton means the un-focused split mirrors the focused one.
///
/// Cleared back to `None` after each body call so non-tab side-panel
/// renders (Telemetry, Inspector, Graphs, …) keep their existing
/// "follow the focused tab" semantics via `active_document`.
#[derive(Resource, Default, Debug, Clone)]
pub struct TabRenderContext {
    pub tab_id: Option<TabId>,
    pub doc: Option<DocumentId>,
    pub drilled_class: Option<String>,
}

impl TabRenderContext {
    /// `(doc, drilled_class)` if a tab body is currently rendering.
    pub fn current(&self) -> Option<(DocumentId, Option<&str>)> {
        self.doc.map(|d| (d, self.drilled_class.as_deref()))
    }
}

/// Free-form drilled-class lookup for `doc` from a Bevy world.
///
/// Render-context-aware: when [`TabRenderContext`] names a tab and
/// that tab is on `doc`, returns its scope. Outside render scope
/// (observers, off-render systems) falls back to
/// [`ModelTabs::drilled_class_for_doc`] (first-tab match).
///
/// **B.3 migration target:** every reader of
/// `DrilledInClassNames.get(doc)` should switch to this helper.
/// Once no `DrilledInClassNames` readers remain, the singleton
/// retires (writes via `sync_active_tab_to_doc` go away too).
pub fn drilled_class_for_doc(
    world: &bevy::prelude::World,
    doc: DocumentId,
) -> Option<String> {
    if let Some(ctx) = world.get_resource::<TabRenderContext>() {
        if let Some(tab_id) = ctx.tab_id {
            let tabs = world.resource::<ModelTabs>();
            if let Some(state) = tabs.get(tab_id) {
                if state.doc == doc {
                    return state.drilled_class.clone();
                }
            }
        }
    }
    world.resource::<ModelTabs>().drilled_class_for_doc(doc)
}

/// Registry of open [`ModelViewPanel`] tabs.
///
/// Keyed by [`TabId`] (allocated from `next_id`). Multiple tabs can
/// reference the same [`DocumentId`] — distinguished by their
/// `drilled_class` slot.
///
/// Closing a tab drops its entry here but *does not* remove the
/// underlying `ModelicaDocument` from [`ModelicaDocumentRegistry`];
/// the registry's lifetime is the union of all tabs viewing it.
#[derive(Resource, Default)]
pub struct ModelTabs {
    tabs: HashMap<TabId, ModelTabState>,
    next_id: u64,
}

impl ModelTabs {
    fn allocate_id(&mut self) -> TabId {
        // Start at 1; 0 is sometimes used as an "unassigned" sentinel
        // by API callers and we don't want a tab id to collide with
        // it. Saturating-add since u64::MAX is a non-issue but the
        // overflow check costs nothing.
        self.next_id = self.next_id.saturating_add(1);
        self.next_id
    }

    // ── Tab lifecycle decision tree ───────────────────────────────
    //
    // Three entry points, three intents. Pick the one that matches
    // the gesture; never invent a fourth.
    //
    // - `ensure_preview_for(doc, drilled)` — **browser single-click**.
    //   Reuses the global preview slot if an existing preview is
    //   showing a different `(doc, drilled)`. Pinned by user edit or
    //   by an explicit pin gesture (right-click → Pin). VS Code's
    //   single-click semantic.
    //
    // - `ensure_for(doc, drilled)` — **deliberate open** (drill-in
    //   from canvas, double-click in browser, New File, Open File).
    //   Produces a *pinned* tab matching `(doc, drilled)`. Reuses an
    //   existing tab with the same key when one is open.
    //
    // - `open_new(doc, drilled)` — **split / open in new view**.
    //   Always allocates a fresh `TabId`; ignores existing tabs on
    //   the same `(doc, drilled)`. Two tabs viewing the same class
    //   each carry their own viewport, selection, and scene state.

    /// Find an existing tab matching `(doc, drilled_class)` or
    /// allocate a fresh one. The newly-created tab is **pinned** by
    /// default — this entry point is for deliberate opens (drill-in,
    /// New, Open File) that should produce a persistent tab. Browser
    /// single-clicks should use [`ensure_preview_for`] instead.
    pub fn ensure_for(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<String>,
    ) -> TabId {
        if let Some((id, _)) = self.tabs.iter().find(|(_, s)| {
            s.doc == doc && s.drilled_class.as_deref() == drilled_class.as_deref()
        }) {
            return *id;
        }
        let id = self.allocate_id();
        self.tabs.insert(
            id,
            ModelTabState {
                doc,
                drilled_class,
                view_mode: ModelViewMode::default(),
                pinned: true,
                load_error: None,
            },
        );
        id
    }

    /// Browser-click entry point with VS Code preview-tab semantics:
    ///
    /// 1. If a tab already matches `(doc, drilled_class)` → focus it
    ///    (no churn, no duplication).
    /// 2. Else if an *unpinned* preview tab exists → repurpose it
    ///    (mutate its `doc`/`drilled_class` in place, keep `view_mode`)
    ///    so casual click-around doesn't pile up dozens of tabs.
    /// 3. Else → allocate a fresh **unpinned** tab.
    ///
    /// Pinning happens automatically on the first edit / drill-in
    /// (see [`pin`]) or explicitly via the tab's right-click menu.
    pub fn ensure_preview_for(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<String>,
    ) -> TabId {
        if let Some((id, _)) = self.tabs.iter().find(|(_, s)| {
            s.doc == doc && s.drilled_class.as_deref() == drilled_class.as_deref()
        }) {
            return *id;
        }
        if let Some((id, state)) = self.tabs.iter_mut().find(|(_, s)| !s.pinned) {
            state.doc = doc;
            state.drilled_class = drilled_class;
            return *id;
        }
        let id = self.allocate_id();
        self.tabs.insert(
            id,
            ModelTabState {
                doc,
                drilled_class,
                view_mode: ModelViewMode::default(),
                pinned: false,
                load_error: None,
            },
        );
        id
    }

    /// Always allocate a fresh tab for `(doc, drilled_class)` even
    /// when one already exists. Used by the "Open in new view"
    /// (split) action — pinned by default since the user
    /// deliberately asked for a duplicate.
    pub fn open_new(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<String>,
    ) -> TabId {
        let id = self.allocate_id();
        self.tabs.insert(
            id,
            ModelTabState {
                doc,
                drilled_class,
                view_mode: ModelViewMode::default(),
                pinned: true,
                load_error: None,
            },
        );
        id
    }

    /// Mark `tab_id` as pinned (no-op if already pinned). Called
    /// from edit hooks and the tab's right-click "Pin" action.
    pub fn pin(&mut self, tab_id: TabId) {
        if let Some(state) = self.tabs.get_mut(&tab_id) {
            state.pinned = true;
        }
    }

    /// Pin every tab currently viewing `doc`. Used by the
    /// edit-side hook that promotes a preview tab to persistent
    /// the moment the user makes any structural change.
    pub fn pin_all_for_doc(&mut self, doc: DocumentId) {
        for state in self.tabs.values_mut() {
            if state.doc == doc {
                state.pinned = true;
            }
        }
    }

    // `ensure(doc)` migration shim deleted in B.4. All callers now
    // use `ensure_for(doc, None)` directly.

    /// Close the specific tab. Returns the tab state if it existed.
    pub fn close_tab(&mut self, tab_id: TabId) -> Option<ModelTabState> {
        self.tabs.remove(&tab_id)
    }

    /// Mutable iterator over `(TabId, &mut ModelTabState)` for every
    /// tab viewing `doc`. Used by writers that previously updated
    /// the legacy `DrilledInClassNames` cache — they now update the
    /// tabs directly (B.3 phase 3 — singleton retire).
    pub fn iter_mut_for_doc(
        &mut self,
        doc: DocumentId,
    ) -> impl Iterator<Item = (TabId, &mut ModelTabState)> + '_ {
        self.tabs
            .iter_mut()
            .filter(move |(_, s)| s.doc == doc)
            .map(|(id, s)| (*id, s))
    }

    /// Drilled class for `doc` derived from the tab table —
    /// authoritative source per the B.3 singleton-retire plan.
    /// Picks the first tab matching `doc` (HashMap iteration order
    /// — best-effort determinism, fine for the common
    /// one-tab-per-doc case).
    ///
    /// Replaces `DrilledInClassNames.get(doc)` reads. Writers into
    /// `ModelTabState.drilled_class` are the new source of truth;
    /// this method derives the same answer without going through
    /// the legacy cache.
    pub fn drilled_class_for_doc(&self, doc: DocumentId) -> Option<String> {
        let tab_id = self.any_for_doc(doc)?;
        self.get(tab_id)?.drilled_class.clone()
    }

    /// Close every tab whose `drilled_class` is `qualified` or a
    /// descendant of it (e.g. `Foo.Bar.Baz` is a descendant of
    /// `Foo.Bar`). Scoped to `doc` so removing `Foo.Bar` in
    /// document A does not close a tab drilled into `Foo.Bar` in
    /// document B.
    ///
    /// Implements **cross-truth rule R4** (see
    /// `docs/architecture/B0_CROSS_TRUTH_POLICY.md`): when a class
    /// is deleted the AST-canonical view dangles, so the matching
    /// tab must close. Caller responsibility: wire from the
    /// `RemoveClass` observer (or `ModelicaChange::ClassRemoved`
    /// dispatch) and clean up companion per-tab state for each
    /// returned id (canvas, editor buffer).
    pub fn close_drilled_into(&mut self, doc: DocumentId, qualified: &str) -> Vec<TabId> {
        if qualified.is_empty() {
            return Vec::new();
        }
        let prefix = format!("{qualified}.");
        let to_close: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|(id, s)| {
                if s.doc != doc {
                    return None;
                }
                let drilled = s.drilled_class.as_deref()?;
                (drilled == qualified || drilled.starts_with(&prefix)).then_some(*id)
            })
            .collect();
        for id in &to_close {
            self.tabs.remove(id);
        }
        to_close
    }

    /// Drop *every* tab pointing at `doc`. Used when a document is
    /// fully closed (registry removal); all of its views must go
    /// with it.
    pub fn close_all_for_doc(&mut self, doc: DocumentId) -> Vec<TabId> {
        let ids: Vec<TabId> = self
            .tabs
            .iter()
            .filter_map(|(id, s)| (s.doc == doc).then_some(*id))
            .collect();
        for id in &ids {
            self.tabs.remove(id);
        }
        ids
    }

    /// Back-compat shim for the old `close(doc)` API.
    pub fn close(&mut self, doc: DocumentId) {
        let _ = self.close_all_for_doc(doc);
    }

    /// Immutable lookup by tab id.
    pub fn get(&self, tab_id: TabId) -> Option<&ModelTabState> {
        self.tabs.get(&tab_id)
    }

    /// Mutable lookup by tab id.
    pub fn get_mut(&mut self, tab_id: TabId) -> Option<&mut ModelTabState> {
        self.tabs.get_mut(&tab_id)
    }

    /// First tab viewing `doc` (any drilled class). Useful for
    /// legacy code that wants "the" tab for a document.
    pub fn any_for_doc(&self, doc: DocumentId) -> Option<TabId> {
        self.tabs
            .iter()
            .find_map(|(id, s)| (s.doc == doc).then_some(*id))
    }

    /// Find a tab matching `(doc, drilled_class)`.
    pub fn find_for(
        &self,
        doc: DocumentId,
        drilled_class: Option<&str>,
    ) -> Option<TabId> {
        self.tabs.iter().find_map(|(id, s)| {
            (s.doc == doc && s.drilled_class.as_deref() == drilled_class).then_some(*id)
        })
    }

    /// Mutable variant of [`find_for`].
    pub fn find_for_mut(
        &mut self,
        doc: DocumentId,
        drilled_class: Option<&str>,
    ) -> Option<&mut ModelTabState> {
        self.tabs.iter_mut().find_map(|(_, s)| {
            (s.doc == doc && s.drilled_class.as_deref() == drilled_class).then_some(s)
        })
    }

    /// Iterate `(tab_id, state)` for all open tabs.
    pub fn iter(&self) -> impl Iterator<Item = (TabId, &ModelTabState)> + '_ {
        self.tabs.iter().map(|(id, s)| (*id, s))
    }

    /// Iterate the **distinct** document ids that have ≥1 tab open.
    /// Drill-in dedup callers use this to avoid re-allocating a doc
    /// for a class that's already on the dock.
    pub fn iter_docs(&self) -> impl Iterator<Item = DocumentId> + '_ {
        let mut seen = std::collections::HashSet::new();
        self.tabs
            .values()
            .filter_map(move |s| seen.insert(s.doc).then_some(s.doc))
    }

    /// Whether any tab is viewing `doc`.
    pub fn contains(&self, doc: DocumentId) -> bool {
        self.any_for_doc(doc).is_some()
    }

    /// Count of tabs viewing `doc`. Lets close-flows decide whether
    /// to remove the underlying document (count was 1) or leave it
    /// alive for the remaining tabs.
    pub fn count_for_doc(&self, doc: DocumentId) -> usize {
        self.tabs.values().filter(|s| s.doc == doc).count()
    }
}

/// The Modelica model-view panel. Zero-sized — per-tab state lives in
/// [`ModelTabs`], the render body delegates to the existing code /
/// diagram panels.
pub struct ModelViewPanel {
    /// Reused renderers for the tab body. The unified toolbar is
    /// rendered by [`render_unified_toolbar`] before dispatching to
    /// one of these based on the tab's current view mode.
    code: CodeEditorPanel,
    canvas: CanvasDiagramPanel,
}

impl Default for ModelViewPanel {
    fn default() -> Self {
        Self {
            code: CodeEditorPanel,
            canvas: CanvasDiagramPanel,
        }
    }
}

impl InstancePanel for ModelViewPanel {
    fn kind(&self) -> PanelId {
        MODEL_VIEW_KIND
    }

    fn default_slot(&self) -> PanelSlot {
        PanelSlot::Center
    }

    fn closable(&self) -> bool {
        true
    }

    fn title(&self, world: &World, instance: u64) -> String {
        // Tab title mirrors VS Code's pattern:
        //   `●` prefix    → unsaved changes
        //   `🔒` prefix   → read-only (Example / library — edits won't save)
        //   *italic-ish*  → preview tab (unpinned). egui's tab strip
        //                   draws the title plain, so we surround the
        //                   text with U+2009 thin spaces and use the
        //                   half-bracket conventions VS Code uses in
        //                   keyboard-only listings: a leading `~`
        //                   marks the preview tab. Cheap visual cue
        //                   without needing a custom RichText pipeline
        //                   at the dock layer.
        let (doc, drilled) = resolve_tab_target(world, instance);
        let (base, dirty, read_only) = resolve_tab_title(world, doc, drilled.as_deref());
        let pinned = world
            .get_resource::<ModelTabs>()
            .and_then(|t| t.get(instance))
            .map(|s| s.pinned)
            .unwrap_or(true);
        let mut prefix = String::new();
        if read_only {
            prefix.push_str("🔒 ");
        }
        if dirty {
            prefix.push_str("● ");
        }
        let body = if prefix.is_empty() {
            base
        } else {
            format!("{prefix}{base}")
        };
        if pinned {
            body
        } else {
            // Curly-quote wrap reads as "preview-y" and survives any
            // monospaced or non-italic font the dock uses.
            format!("‹ {body} ›")
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World, instance: u64) {
        let tab_id: TabId = instance;

        // Resolve `(doc, drilled_class)` for this tab. If the tab id
        // isn't yet registered (the workbench mounted us before our
        // creator could record state — vanishingly rare path), bail
        // gracefully rather than panicking.
        let Some((doc, drilled)) = world
            .resource::<ModelTabs>()
            .get(tab_id)
            .map(|s| (s.doc, s.drilled_class.clone()))
        else {
            return;
        };

        // Sync the editor buffer + diagram_dirty flag to this tab
        // before rendering. (Drilled scope + most other state now
        // derive from `ModelTabs` / registry directly.)
        sync_active_tab_to_doc(world, doc, drilled.as_deref());

        // Read the tab's desired view mode so the toolbar can reflect
        // (and, on click, mutate) it.
        let view_mode = world
            .resource::<ModelTabs>()
            .get(tab_id)
            .map(|s| s.view_mode)
            .unwrap_or_default();

        let new_view_mode = render_unified_toolbar(doc, view_mode, ui, world);
        if new_view_mode != view_mode {
            // R3 (B0_CROSS_TRUTH_POLICY.md): if the user is leaving
            // Text mode while the editor still has uncommitted bytes,
            // force-flush via the same `EditText` op the debounce
            // timer would have run. The mode switch then activates
            // *after* the op lands, so the canvas tab's first render
            // observes the new generation.
            //
            // No prompt: the buffer is committed, not discarded —
            // identical semantics to a debounced commit, just earlier.
            if view_mode == ModelViewMode::Text {
                let pending = world
                    .get_resource::<EditorBufferState>()
                    .map(|b| b.pending_commit_at.is_some())
                    .unwrap_or(false);
                if pending {
                    crate::ui::panels::code_editor::commit_pending_buffer(world, doc);
                }
            }
            if let Some(state) = world.resource_mut::<ModelTabs>().get_mut(tab_id) {
                state.view_mode = new_view_mode;
            }
        }

        ui.separator();

        // Persistent read-only strip — rendered for every library
        // / MSL tab regardless of which view (Text / Canvas /
        // Icon / Docs) is active. The old behaviour was to
        // silently discard user edit ops and write a hint into
        // Diagnostics, but most edit gestures (inspector field
        // focus, keyboard typing, drag preview) never reach the
        // ops layer — so the user saw nothing when they tried to
        // modify something. A visible strip with a Duplicate
        // button is unmissable and one click from the fix.
        // B.3 phase 6: derive from registry.
        let tab_read_only = crate::ui::state::read_only_for(world, doc);
        if tab_read_only {
            let mut banner_duplicate_clicked = false;
            egui::Frame::NONE
                .fill(egui::Color32::from_rgb(60, 48, 20))
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("🔒")
                                .color(egui::Color32::from_rgb(220, 200, 120))
                                .size(14.0),
                        );
                        ui.label(
                            egui::RichText::new(
                                "Read-only library model — \
                                 edits won't stick. Duplicate it \
                                 to your workspace to make changes.",
                            )
                            .color(egui::Color32::from_rgb(220, 200, 120))
                            .size(12.0),
                        );
                        ui.add_space(ui.available_width() - 170.0);
                        if ui
                            .button("📄  Duplicate to edit")
                            .on_hover_text(
                                "Create an editable Untitled copy \
                                 of this class — the MSL original \
                                 stays untouched.",
                            )
                            .clicked()
                        {
                            banner_duplicate_clicked = true;
                        }
                    });
                });
            if banner_duplicate_clicked {
                world
                    .commands()
                    .trigger(crate::ui::commands::DuplicateModelFromReadOnly {
                        source_doc: doc,
                    });
            }
        }

        // Body — delegate to the existing code / diagram panels.
        // Each panel reads its inputs from the registry by `doc`
        // and from `EditorBufferState` (which `sync_active_tab_to_doc`
        // just pointed at this tab's document).
        // Diagnostic: log on first render per tab (view switches
        // don't re-log — one-shot per tab open) so we can see which
        // body path the freeze is hitting. Throw-away; promoted to
        // a Diagnostics event if this turns out to be the culprit.
        {
            use std::sync::{Mutex, OnceLock};
            static SEEN: OnceLock<Mutex<std::collections::HashSet<(u64, u8)>>> =
                OnceLock::new();
            let seen = SEEN.get_or_init(|| Mutex::new(Default::default()));
            let tag = match new_view_mode {
                ModelViewMode::Text => 0u8,
                ModelViewMode::Canvas => 1,
                ModelViewMode::Icon => 2,
                ModelViewMode::Docs => 3,
            };
            if let Ok(mut s) = seen.lock() {
                if s.insert((doc.raw(), tag)) {
                    bevy::log::info!(
                        "[ModelView] rendering tab doc={:?} mode={:?}",
                        doc,
                        new_view_mode,
                    );
                }
            }
        }

        // Publish this tab's identity to the body for the duration
        // of the body render. Bodies that key per-tab state (canvas
        // viewport / scene cache, editor buffer) read from this
        // resource instead of `WorkspaceResource.active_document`,
        // so two splits each see their own tab. Restored to the
        // previous value after the body returns so re-entrant
        // renders (shouldn't happen, but cheap to guard) don't
        // strand stale state.
        let prev_ctx = world.resource::<TabRenderContext>().clone();
        {
            let mut ctx = world.resource_mut::<TabRenderContext>();
            ctx.tab_id = Some(tab_id);
            ctx.doc = Some(doc);
            ctx.drilled_class = drilled.clone();
        }
        match new_view_mode {
            ModelViewMode::Text => self.code.render(ui, world),
            ModelViewMode::Canvas => self.canvas.render(ui, world),
            ModelViewMode::Icon => render_icon_view(ui, world),
            ModelViewMode::Docs => render_docs_view(ui, world),
        }
        *world.resource_mut::<TabRenderContext>() = prev_ctx;
    }

    /// Right-click on a model tab → VS Code-style menu:
    ///
    /// - **Pin** / **Unpin** — toggles the preview-tab state.
    ///   Pinned tabs survive the next browser click.
    /// - **Open in new view** — clones the tab (same `doc` +
    ///   `drilled_class`) into a fresh, pinned tab. The user then
    ///   drags it into a split or switches its view mode (Text /
    ///   Diagram / Icon / Docs) to create the side-by-side layout
    ///   they're after — the canonical Canvas-+-Text recipe.
    fn tab_context_menu(
        &mut self,
        ui: &mut egui::Ui,
        world: &mut World,
        instance: u64,
    ) {
        let tab_id: TabId = instance;
        let (doc, drilled, pinned) = match world
            .resource::<ModelTabs>()
            .get(tab_id)
            .map(|s| (s.doc, s.drilled_class.clone(), s.pinned))
        {
            Some(t) => t,
            None => return,
        };

        if ui
            .button(if pinned { "📌 Unpin" } else { "📌 Pin tab" })
            .on_hover_text(
                "Pinned tabs survive the next browser click — \
                 unpinned (preview) tabs get replaced.",
            )
            .clicked()
        {
            if let Some(state) = world.resource_mut::<ModelTabs>().get_mut(tab_id) {
                state.pinned = !pinned;
            }
            ui.close();
        }

        ui.separator();

        if ui
            .button("🪟 Open in new view")
            .on_hover_text(
                "Open this same model in a second tab. Drag it to a \
                 dock edge to make a split, then switch one to Text \
                 to get a side-by-side Canvas + Text view.",
            )
            .clicked()
        {
            let new_id = world
                .resource_mut::<ModelTabs>()
                .open_new(doc, drilled.clone());
            world.commands().trigger(lunco_workbench::OpenTab {
                kind: MODEL_VIEW_KIND,
                instance: new_id,
            });
            ui.close();
        }
    }
}

/// Resolve the `(doc, drilled_class)` pair this tab views from its
/// stable [`TabId`]. Returns `(DocumentId::new(instance), None)` as
/// a fallback for tab ids that haven't been registered yet — the
/// workbench occasionally calls `title()` on freshly-restored
/// layout entries before the creator has filled in [`ModelTabs`].
fn resolve_tab_target(world: &World, instance: u64) -> (DocumentId, Option<String>) {
    if let Some(state) = world.get_resource::<ModelTabs>().and_then(|t| t.get(instance)) {
        return (state.doc, state.drilled_class.clone());
    }
    (DocumentId::new(instance), None)
}

/// Compute `(base, dirty, read_only)` for `doc` scoped to
/// `drilled_class` when present. The tab's `InstancePanel::title`
/// prefixes icons accordingly.
fn resolve_tab_title(
    world: &World,
    doc: DocumentId,
    drilled_class: Option<&str>,
) -> (String, bool, bool) {
    if let Some(host) = world
        .get_resource::<ModelicaDocumentRegistry>()
        .and_then(|r| r.host(doc))
    {
        let document = host.document();
        // Drilled-in tabs (from OpenClass / the canvas's double-click
        // gesture) back onto a raw `.mo` file — often a package
        // aggregate like `Continuous.mo` that holds Der/PID/FirstOrder
        // side by side. The file's display name ("Continuous") then
        // hides *which* class the user drilled into. Prefer the tab's
        // own `drilled_class` short name when present.
        let base = drilled_class
            .and_then(|qualified| qualified.rsplit('.').next().map(str::to_string))
            .unwrap_or_else(|| {
                // MSL packages live in `package.mo` files — the raw
                // basename "package" is meaningless to the user (every
                // package has it), so fall back to the parent folder
                // name (`Continuous`, `Examples`, …) which is the
                // package's actual short name.
                let raw = document.origin().display_name();
                if raw == "package" {
                    if let lunco_doc::DocumentOrigin::File { path, .. } =
                        document.origin()
                    {
                        if let Some(parent) = path
                            .parent()
                            .and_then(|p| p.file_name())
                            .and_then(|s| s.to_str())
                        {
                            return parent.to_string();
                        }
                    }
                }
                raw
            });
        return (base, document.is_dirty(), document.is_read_only());
    }

    // Display name + read_only derive from the registry directly.
    // Active-doc identity is the Workspace's concern.
    let active_doc = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document);
    if active_doc == Some(doc) {
        if let Some(name) = crate::ui::state::display_name_for(world, doc) {
            return (name, false, crate::ui::state::read_only_for(world, doc));
        }
    }
    (format!("Model #{}", doc.raw()), false, false)
}

/// Point `editor_buffer` / `diagram_dirty` / `selected_entity` at
/// `doc`, loading source from the registry.
///
/// Fast-path skip when `EditorBufferState.text` length already
/// matches the live source for this doc — avoids `diagram_dirty`
/// spam on re-render. Mutates `selected_entity` to one of the
/// entities linked to `doc`, if any — that's what the Telemetry /
/// Inspector / Graphs side panels filter by.
pub(crate) fn sync_active_tab_to_doc(
    world: &mut World,
    doc: DocumentId,
    drilled_class: Option<&str>,
) {
    // B.3 phase 3: legacy `DrilledInClassNames` cache mirror
    // removed. `ModelTabState.drilled_class` is now authoritative
    // and readers go through
    // `crate::ui::panels::model_view::drilled_class_for_doc`.
    let _ = drilled_class;
    // Already active AND the cached snapshot is from the real doc
    // (not a placeholder that filled in while a drill-in load was
    // still in flight). The check on non-empty source distinguishes:
    //   - Real snapshot: source is the file contents → nothing to do.
    //   - Placeholder: source is "" because host was still missing
    //     when we last synced → refresh now that the registry has
    //     the real document.
    //
    // Without the second condition, drill-in tabs could get stuck
    // showing an empty Text view forever: sync runs with a placeholder,
    // `already_active` fires, we short-circuit, and the real
    // source never lands until the user manually switches tabs.
    let active_matches = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
        == Some(doc);
    // Fast-path: if the buffer's text length matches the live
    // source for this doc and we're already active, nothing to do.
    let buffer_matches_live = {
        let live = world
            .resource::<ModelicaDocumentRegistry>()
            .host(doc)
            .map(|h| h.document().source().len())
            .unwrap_or(0);
        let buf_len = world
            .get_resource::<EditorBufferState>()
            .map(|b| b.text.len())
            .unwrap_or(0);
        live > 0 && live == buf_len
    };
    if active_matches && buffer_matches_live {
        refresh_selected_entity_for(world, doc);
        return;
    }

    // Gather read-side data up front so we don't hold two borrows
    // at once. `detected_name` comes from the doc's cached AST —
    // MUST NOT re-parse here. Previously this called
    // `ast_extract::extract_model_name(source)` which kicked off an
    // uncached rumoca parse on the main thread; on a 184 KB
    // drill-in source that froze the UI for ~200 s in debug builds.
    let snapshot = {
        let registry = world.resource::<ModelicaDocumentRegistry>();
        registry.host(doc).map(|h| {
            let document = h.document();
            let display_name = document.origin().display_name();
            let path_str = document
                .canonical_path()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("mem://{display_name}"));
            // Classify for Package Browser / UI badges — we've lost
            // the MSL-vs-Bundled distinction at the doc level (both
            // are just read-only files now); Package Browser-side
            // code that *needs* that distinction should consult its
            // own origin-tracking.
            let library = match document.origin() {
                lunco_doc::DocumentOrigin::Untitled { .. } => {
                    crate::ui::state::ModelLibrary::InMemory
                }
                lunco_doc::DocumentOrigin::File { writable: true, .. } => {
                    crate::ui::state::ModelLibrary::User
                }
                lunco_doc::DocumentOrigin::File { writable: false, .. } => {
                    crate::ui::state::ModelLibrary::Bundled
                }
            };
            // `document.is_read_only()` means "can't Save without
            // Save-As" — true for Untitled docs despite Untitled
            // being fully editable. For UI purposes (right-click
            // menu, apply_ops gate) "read-only" means "library
            // class the user isn't allowed to mutate", so tie it
            // to the library classification instead: only Bundled
            // (MSL, drill-in target) is read-only; Untitled and
            // User files are both editable.
            let read_only =
                matches!(library, crate::ui::state::ModelLibrary::Bundled);
            // First non-package class via the Index. Sees optimistic
            // patches and avoids walking the AST.
            let detected_name = document
                .index()
                .classes
                .values()
                .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                .map(|c| c.name.clone());
            (
                path_str,
                display_name,
                document.source().to_string(),
                read_only,
                library,
                detected_name,
            )
        })
    };

    // Fallback: the doc is a placeholder reserved by drill-in and
    // its bg load hasn't finished yet (so `registry.host(doc)` is
    // still None). We still need to flip `WorkspaceResource.active_document`
    // to this tab's id — otherwise every per-doc lookup downstream
    // (canvas state, loading overlay, read-only gate) keeps
    // routing to the PREVIOUS tab's doc and the new tab visually
    // mirrors it until the parse completes. Use the DrillInLoads
    // entry for a display name; the source stays empty until the
    // real document is installed.
    let snapshot = snapshot.or_else(|| {
        // Drill-in tab still loading? Use the qualified name as
        // the placeholder identity.
        if let Some(loads) = world
            .get_resource::<crate::ui::panels::canvas_diagram::DrillInLoads>()
        {
            if let Some(qualified) = loads.detail(doc) {
                let qualified = qualified.to_string();
                let short = qualified
                    .rsplit('.')
                    .next()
                    .map(str::to_string)
                    .unwrap_or_else(|| qualified.clone());
                return Some((
                    format!("msl://{qualified}"),
                    short.clone(),
                    String::new(),
                    true,
                    crate::ui::state::ModelLibrary::Bundled,
                    Some(short),
                ));
            }
        }
        // Duplicate-to-workspace tab still building? Use the target
        // display name; the copy is editable (not read-only).
        if let Some(dup) = world
            .get_resource::<crate::ui::panels::canvas_diagram::DuplicateLoads>()
        {
            if let Some(display) = dup.detail(doc) {
                let display = display.to_string();
                return Some((
                    format!("mem://{display}"),
                    display.clone(),
                    String::new(),
                    false,
                    crate::ui::state::ModelLibrary::InMemory,
                    Some(display),
                ));
            }
        }
        None
    });
    let Some((path_str, display_name, source, read_only, library, detected_name)) =
        snapshot
    else {
        return;
    };

    // Compute line starts for the editor buffer.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, b) in source.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(i + 1);
        }
    }

    // Refresh `editor_buffer` mirror + `diagram_dirty`.
    let _ = (display_name, read_only, library);
    {
        let source_arc: std::sync::Arc<str> = source.clone().into();
        let mut state = world.resource_mut::<WorkbenchState>();
        state.editor_buffer = source_arc.to_string();
        state.diagram_dirty = true;
    }

    // Workspace is the single source of truth for "which doc has
    // focus" — flip its pointer to this tab's doc.
    {
        let mut ws = world.resource_mut::<lunco_workbench::WorkspaceResource>();
        ws.active_document = Some(doc);
    }

    // Editor buffer carries typed `bound_doc` identity used by
    // `package_browser`'s stale-buffer check.
    {
        let mut buf = world.resource_mut::<EditorBufferState>();
        buf.text = source;
        buf.line_starts = line_starts.into();
        buf.detected_name = detected_name;
        buf.model_path = path_str.clone();
        buf.bound_doc = Some(doc);
    }

    // The canvas viewer reprojects from the document AST every frame
    // when the generation advances, so there is no per-tab cache to
    // wipe on doc switch. Compile-status comes from
    // `WorkbenchState.compilation_error` and is reset when the next
    // compile starts.
    refresh_selected_entity_for(world, doc);
}

/// Point `WorkbenchState.selected_entity` at one of the entities
/// linked to `doc`, if any. No-op if nothing is linked yet — the
/// side panels will show empty state until a compile spawns one.
fn refresh_selected_entity_for(world: &mut World, doc: DocumentId) {
    let entity = world
        .resource::<ModelicaDocumentRegistry>()
        .entities_linked_to(doc)
        .into_iter()
        .next();
    if let Some(entity) = entity {
        if let Some(mut state) = world.get_resource_mut::<WorkbenchState>() {
            if state.selected_entity != Some(entity) {
                state.selected_entity = Some(entity);
            }
        }
    }
}

/// Render the unified per-tab toolbar. Returns the (possibly updated)
/// view mode the caller should persist into [`ModelTabs`].
fn render_unified_toolbar(
    doc: DocumentId,
    view_mode: ModelViewMode,
    ui: &mut egui::Ui,
    world: &mut World,
) -> ModelViewMode {
    let tokens = world
        .get_resource::<lunco_theme::Theme>()
        .map(|t| t.tokens.clone())
        .unwrap_or_else(|| lunco_theme::Theme::dark().tokens.clone());
    // Snapshot everything we need before the closure so we don't
    // fight the borrow checker mid-layout.
    // B.3 phase 6: derive from registry / origin.
    let display_name = crate::ui::state::display_name_for(world, doc)
        .unwrap_or_else(|| format!("Model #{}", doc.raw()));

    let compile_state = world.resource::<CompileStates>().state_of(doc);
    let is_read_only = crate::ui::state::read_only_for(world, doc);
    // Icon-only class detection — read the origin's display name
    // (carries `msl://Foo` for drill-in tabs).
    let is_icon_only_tab = crate::ui::loaded_classes::is_icon_only_class(&display_name)
        || display_name.contains("/Icons/");
    // B.3 phase 4: per-doc error on CompileStates.
    let compilation_error = world
        .get_resource::<crate::ui::CompileStates>()
        .and_then(|cs| cs.error_for(doc).map(str::to_string));

    let undo_redo = world
        .resource::<ModelicaDocumentRegistry>()
        .host(doc)
        .map(|h| (h.can_undo(), h.can_redo(), h.undo_depth(), h.redo_depth()));

    // Live simulation state for the entity linked to this doc, if any.
    // Populated after a successful Compile; `None` means the toolbar's
    // Run/Pause/Reset group is still disabled (there's no stepper to
    // drive). Snapshot the fields we need so the closure below
    // doesn't need to re-query the world mid-render.
    let sim_state: Option<(bool, f64)> = world
        .resource::<ModelicaDocumentRegistry>()
        .entities_linked_to(doc)
        .into_iter()
        .next()
        .and_then(|e| {
            world
                .get::<crate::ModelicaModel>(e)
                .map(|m| (m.paused, m.current_time))
        });

    // Collect button presses without touching world inside the closure.
    let mut compile_clicked = false;
    let mut fast_run_clicked = false;
    let mut undo_clicked = false;
    let mut redo_clicked = false;
    let mut dismiss_error = false;
    let mut duplicate_clicked = false;
    let mut auto_arrange_clicked = false;
    let mut run_pause_clicked = false;
    let mut reset_clicked = false;
    let mut new_view_mode = view_mode;

    // Always show emoji-only labels in the toolbar. The prior
    // text-bearing form clipped to "Ico"/"dle"/"mpile" once the tab
    // narrowed, and `available_width` proved unreliable as a
    // threshold (the dock layout's reported width didn't match the
    // visual clipping point). Tooltips spell out each button's
    // meaning so nothing is lost.
    let compact = true;
    ui.horizontal(|ui| {
        // Identity is on the tab title now (dirty dot there too);
        // the toolbar shows just the view switcher + status + actions
        // so the header stays tight like VS Code.
        let _ = display_name;
        if is_read_only {
            ui.colored_label(
                tokens.warning,
                if compact { "👁" } else { "👁 read-only" },
            )
            .on_hover_text("Read-only — Duplicate to Workspace to edit");
            ui.separator();
        }

        let text_sel = view_mode == ModelViewMode::Text;
        let canv_sel = view_mode == ModelViewMode::Canvas;
        let icon_sel = view_mode == ModelViewMode::Icon;
        let docs_sel = view_mode == ModelViewMode::Docs;
        // All four views are always available — OMEdit/Dymola
        // pattern. A partial or icon-only class has a legitimately
        // empty Diagram layer, and users should be able to view it.
        // The smart "land in the right view by default" happens at
        // install time (see `drive_drill_in_loads`), not by hiding
        // buttons.
        let _ = (is_read_only, is_icon_only_tab);
        // Emoji-only tabs to keep the toolbar compact at every dock
        // width. Tooltips spell out the meaning.
        if ui
            .selectable_label(text_sel, "📝")
            .on_hover_text("Text view (source)")
            .clicked()
        {
            new_view_mode = ModelViewMode::Text;
        }
        if ui
            .selectable_label(canv_sel, "🔗")
            .on_hover_text("Diagram view (canvas)")
            .clicked()
        {
            new_view_mode = ModelViewMode::Canvas;
        }
        if ui
            .selectable_label(icon_sel, "🎨")
            .on_hover_text("Icon view (class symbol)")
            .clicked()
        {
            new_view_mode = ModelViewMode::Icon;
        }
        if ui
            .selectable_label(docs_sel, "📖")
            .on_hover_text("Documentation view")
            .clicked()
        {
            new_view_mode = ModelViewMode::Docs;
        }
        ui.separator();

        if let Some(ref err) = compilation_error {
            let chip = ui
                .colored_label(egui::Color32::LIGHT_RED, if compact { "⚠" } else { "⚠ Error" })
                .on_hover_text(err);
            if chip.clicked() {
                dismiss_error = true;
            }
        } else {
            match compile_state {
                CompileState::Compiling => {
                    ui.colored_label(
                        tokens.warning,
                        if compact { "⏳" } else { "⏳ Compiling…" },
                    )
                    .on_hover_text("Compiling…");
                }
                CompileState::Ready => {
                    ui.colored_label(
                        tokens.success,
                        if compact { "✓" } else { "✓ Ready" },
                    )
                    .on_hover_text("Ready");
                }
                CompileState::Error => {
                    ui.colored_label(
                        tokens.error,
                        if compact { "⚠" } else { "⚠ Error" },
                    )
                    .on_hover_text("Compile error");
                }
                CompileState::Idle => {
                    ui.colored_label(
                        tokens.text_subdued,
                        if compact { "◌" } else { "◌ Idle" },
                    )
                    .on_hover_text("Idle");
                }
            }
        }

        if let Some((can_undo, can_redo, undo_n, redo_n)) = undo_redo {
            ui.separator();
            undo_clicked = ui
                .add_enabled(can_undo, egui::Button::new("↶"))
                .on_hover_text(format!("Undo — {undo_n} available (Ctrl+Z)"))
                .clicked();
            redo_clicked = ui
                .add_enabled(can_redo, egui::Button::new("↷"))
                .on_hover_text(format!("Redo — {redo_n} available (Ctrl+Shift+Z)"))
                .clicked();
        }

        ui.separator();
        // Compile is independent of writability — simulating a
        // read-only Example is a valid (and common) workflow. Save
        // stays gated on writable; Compile only waits for an
        // in-flight compile to settle.
        let runner_busy = world
            .get_resource::<crate::ModelicaRunnerResource>()
            .map(|r| r.0.is_busy())
            .unwrap_or(false);
        let compile_enabled = !matches!(compile_state, CompileState::Compiling) && !runner_busy;
        compile_clicked = ui
            .add_enabled(
                compile_enabled,
                egui::Button::new(if compact { "🚀" } else { "🚀 Compile" }),
            )
            .on_hover_text("Compile the current model and run it (F5)")
            .clicked();

        // Fast Run — batch simulation off-thread (Web Worker on wasm,
        // std::thread on native). Independent of the realtime
        // Interactive stepping. Bounds come from the model's
        // `experiment(...)` annotation; fallback 0..1.
        // See docs/architecture/25-experiments.md.
        let fast_enabled = !matches!(compile_state, CompileState::Compiling) && !runner_busy;
        let fast_label = if compact {
            "⏩".to_string()
        } else if runner_busy {
            "⏩ Running…".to_string()
        } else {
            "⏩ Fast".to_string()
        };
        fast_run_clicked = ui
            .add_enabled(fast_enabled, egui::Button::new(fast_label))
            .on_hover_text(
                "Fast Run: compile + simulate end-to-end as fast as possible. \
                 Result lands in the Experiments registry; multiple runs can be \
                 plotted together. Bounds default from the model's experiment(...) \
                 annotation.",
            )
            .clicked();

        // Inline bounds + class readout — surfaces what Fast Run will
        // do without forcing the user to open the Experiments panel.
        // Reads the same draft + runner-default chain that
        // FastRunActiveModel uses, so what's shown is what runs.
        let drilled_class = world
            .get_resource::<crate::ui::panels::model_view::ModelTabs>()
            .and_then(|t| t.drilled_class_for_doc(doc));
        let model_ref_for_readout: Option<lunco_experiments::ModelRef> = drilled_class
            .clone()
            .or_else(|| {
                world
                    .get_resource::<crate::ui::state::ModelicaDocumentRegistry>()
                    .and_then(|r| r.host(doc))
                    .and_then(|h| {
                        h.document()
                            .index()
                            .classes
                            .values()
                            .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                            .map(|c| c.name.clone())
                    })
            })
            .map(lunco_experiments::ModelRef);
        if let Some(model_ref) = model_ref_for_readout {
            let drafted = world
                .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                .and_then(|d| d.get(&model_ref).and_then(|dr| dr.bounds_override.clone()));
            let bounds = drafted.unwrap_or_else(|| {
                world
                    .get_resource::<crate::ModelicaRunnerResource>()
                    .and_then(|r| {
                        use lunco_experiments::ExperimentRunner;
                        r.0.default_bounds(&model_ref)
                    })
                    .unwrap_or(lunco_experiments::RunBounds {
                        t_start: 0.0,
                        t_end: 1.0,
                        dt: None,
                        tolerance: None,
                        solver: None,
                    })
            });
            if !compact {
                let dt_text = match bounds.dt {
                    Some(d) => format!("dt={d:.3}"),
                    None => "dt=auto".into(),
                };
                ui.label(
                    egui::RichText::new(format!(
                        "{:.2}→{:.2}s {}",
                        bounds.t_start, bounds.t_end, dt_text
                    ))
                    .monospace()
                    .size(11.0)
                    .color(tokens.text_subdued),
                )
                .on_hover_text("Fast Run bounds — edit in 🧪 Experiments → ⚙ Overrides + Bounds");
                if let Some(class) = &drilled_class {
                    ui.label(
                        egui::RichText::new(format!("· {}", class))
                            .size(11.0)
                            .color(tokens.text_subdued),
                    )
                    .on_hover_text("Class that Fast Run will simulate");
                }
            }
        }

        // Run-control group. Only meaningful once a stepper exists
        // (i.e. the model compiled and linked a ModelicaModel
        // component). Before that, the worker has nothing to pause or
        // reset — we keep the group disabled rather than hiding it so
        // the toolbar layout stays stable across compile transitions.
        if let Some((paused, t_now)) = sim_state {
            ui.separator();
            // Single toggle: ▶ when paused, ⏸ when running. Mirrors
            // Dymola's sim-tab play/pause (and every video player).
            let (glyph, tip) = if paused {
                ("▶", "Resume stepping")
            } else {
                ("⏸", "Pause stepping (state preserved — Resume to continue)")
            };
            run_pause_clicked = ui.button(glyph).on_hover_text(tip).clicked();
            reset_clicked = ui
                .button("⟲")
                .on_hover_text("Reset simulation to t=0 (keeps compiled model)")
                .clicked();
            // Clock readout. Mono digits so the width doesn't dance
            // as the number grows; weak colour so it reads as a
            // status line, not a control.
            ui.label(
                egui::RichText::new(format!("t={:.3}s", t_now))
                    .monospace()
                    .weak(),
            )
            .on_hover_text("Simulation time (seconds since t=0)");
        }

        // Auto-Arrange: batch SetPlacement on every component in the
        // active class to a clean grid. Only useful on the Diagram
        // view and only on editable docs. Dymola's "Edit → Auto
        // Arrange" in one button.
        if view_mode == ModelViewMode::Canvas && !is_read_only {
            ui.separator();
            // ▦ (U+25A6) instead of 🧹 — the broom emoji is in the
            // SMP and renders as tofu without a colour-emoji font;
            // the geometric "square with grid" sits in the basic
            // multilingual plane and reads visually as "lay out on a
            // grid" — which is what Auto-Arrange does.
            auto_arrange_clicked = ui
                .button(if compact { "▦" } else { "▦ Auto-Arrange" })
                .on_hover_text(
                    "Lay out all components in a grid and write the \
                     positions back into the source as Placement \
                     annotations. Undo-able.",
                )
                .clicked();
        }

        // Duplicate-to-workspace: only offered on read-only tabs.
        // Users browsing an MSL Example who want to tweak parameters
        // hit this to get an editable copy in a new tab; the library
        // original stays untouched. Mirrors Dymola's "make your own
        // copy" workflow for example models.
        if is_read_only {
            ui.separator();
            duplicate_clicked = ui
                .button(if compact { "📄" } else { "📄 Duplicate to Workspace" })
                .on_hover_text(
                    "Copy this library class into a new editable Untitled \
                     model so you can tweak parameters / connections \
                     without modifying the MSL source.",
                )
                .clicked();
        }
    });

    // Apply side effects after the closure.
    if dismiss_error {
        // B.3 phase 4: per-doc error on CompileStates.
        if let Some(mut cs) = world.get_resource_mut::<crate::ui::CompileStates>() {
            cs.clear_error(doc);
        }
    }
    if undo_clicked {
        world.commands().trigger(lunco_doc_bevy::UndoDocument { doc });
    }
    if redo_clicked {
        world.commands().trigger(lunco_doc_bevy::RedoDocument { doc });
    }
    if duplicate_clicked {
        world
            .commands()
            .trigger(crate::ui::commands::DuplicateModelFromReadOnly {
                source_doc: doc,
            });
    }
    if run_pause_clicked {
        // sim_state was Some to render the button, so unwrap is safe.
        let paused = sim_state.map(|(p, _)| p).unwrap_or(false);
        if paused {
            world
                .commands()
                .trigger(crate::ui::commands::ResumeActiveModel { doc });
        } else {
            world
                .commands()
                .trigger(crate::ui::commands::PauseActiveModel { doc });
        }
    }
    if reset_clicked {
        world
            .commands()
            .trigger(crate::ui::commands::ResetActiveModel { doc });
    }
    if auto_arrange_clicked {
        world
            .commands()
            .trigger(crate::ui::commands::AutoArrangeDiagram { doc });
    }
    if fast_run_clicked {
        // Open the Simulation Setup dialog instead of dispatching
        // directly. The dialog confirms bounds (prefilled from
        // annotation defaults / existing draft), then dispatches
        // FastRunActiveModel on Run.
        let model_ref = world
            .get_resource::<crate::ui::panels::model_view::ModelTabs>()
            .and_then(|t| t.drilled_class_for_doc(doc))
            .or_else(|| {
                world
                    .get_resource::<crate::ui::state::ModelicaDocumentRegistry>()
                    .and_then(|r| r.host(doc))
                    .and_then(|h| {
                        h.document()
                            .index()
                            .classes
                            .values()
                            .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                            .map(|c| c.name.clone())
                    })
            })
            .map(lunco_experiments::ModelRef);
        if let Some(model_ref) = model_ref {
            // Bounds: existing draft override > runner annotation
            // default > 0..1 fallback.
            let drafted_bounds = world
                .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                .and_then(|d| d.get(&model_ref).and_then(|dr| dr.bounds_override.clone()));
            let bounds = drafted_bounds.unwrap_or_else(|| {
                world
                    .get_resource::<crate::ModelicaRunnerResource>()
                    .and_then(|r| {
                        use lunco_experiments::ExperimentRunner;
                        r.0.default_bounds(&model_ref)
                    })
                    .unwrap_or(lunco_experiments::RunBounds {
                        t_start: 0.0,
                        t_end: 1.0,
                        dt: None,
                        tolerance: None,
                        solver: None,
                    })
            });
            let overrides_count = world
                .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                .and_then(|d| d.get(&model_ref).map(|dr| dr.overrides.len()))
                .unwrap_or(0);

            // Detect inputs from current source + prefill from draft.
            let source_text = world
                .get_resource::<crate::ui::state::ModelicaDocumentRegistry>()
                .and_then(|r| r.host(doc))
                .map(|h| h.document().source().to_string())
                .unwrap_or_default();
            let detected = crate::experiments_runner::detect_top_level_inputs(&source_text);
            let prefilled = world
                .get_resource::<crate::experiments_runner::ExperimentDrafts>()
                .and_then(|d| d.get(&model_ref).map(|dr| dr.inputs.clone()))
                .unwrap_or_default();
            let inputs: Vec<crate::ui::commands::FastRunInput> = detected
                .into_iter()
                .map(|d| {
                    let value_text = prefilled
                        .get(&lunco_experiments::ParamPath(d.name.clone()))
                        .map(|v| match v {
                            lunco_experiments::ParamValue::Real(x) => format!("{x}"),
                            lunco_experiments::ParamValue::Int(x) => format!("{x}"),
                            lunco_experiments::ParamValue::Bool(b) => {
                                if *b { "true".into() } else { "false".into() }
                            }
                            lunco_experiments::ParamValue::String(s) => s.clone(),
                            lunco_experiments::ParamValue::Enum(s) => s.clone(),
                            lunco_experiments::ParamValue::RealArray(_) => "(array)".into(),
                        })
                        .unwrap_or_default();
                    crate::ui::commands::FastRunInput {
                        name: d.name,
                        type_name: d.type_name,
                        value_text,
                    }
                })
                .collect();

            if let Some(mut setup) = world
                .get_resource_mut::<crate::ui::commands::FastRunSetupState>()
            {
                setup.0 = Some(crate::ui::commands::FastRunSetupEntry {
                    doc,
                    model_ref,
                    bounds,
                    overrides_count,
                    inputs,
                });
            }
        } else {
            // Resolution fallback — dispatch directly so the existing
            // class-picker (or "no class" warning) fires.
            world
                .commands()
                .trigger(crate::ui::commands::FastRunActiveModel { doc });
        }
    }
    if compile_clicked {
        match new_view_mode {
            ModelViewMode::Text => {
                let buffer = world.resource::<EditorBufferState>().text.clone();
                if !buffer.is_empty() {
                    world
                        .resource_mut::<ModelicaDocumentRegistry>()
                        .checkpoint_source(doc, buffer);
                }
                world
                    .commands()
                    .trigger(crate::ui::commands::CompileActiveModel {
                        doc,
                        class: String::new(),
                    });
            }
            ModelViewMode::Canvas => {
                // Canvas is a read-only view in B2 — compile just
                // routes through the document source, same as Text.
                // B3 (doc write-back) will emit real ops from drag /
                // connect; compile can then stay the same.
                world
                    .commands()
                    .trigger(crate::ui::commands::CompileActiveModel {
                        doc,
                        class: String::new(),
                    });
            }
            ModelViewMode::Icon => {
                // Icon is a pure display view — compile-from-icon
                // doesn't mean anything, route through the document
                // source the same as Text does.
                world
                    .commands()
                    .trigger(crate::ui::commands::CompileActiveModel {
                        doc,
                        class: String::new(),
                    });
            }
            ModelViewMode::Docs => {
                // Docs is pure display — compile routes through the
                // document source like Text.
                world
                    .commands()
                    .trigger(crate::ui::commands::CompileActiveModel {
                        doc,
                        class: String::new(),
                    });
            }
        }
    }
    new_view_mode
}

/// Render the class's icon. Priority order:
///
/// 1. MSL-registered class: look up its `icon_asset` in
///    `msl_component_library` by qualified name (from the doc's
///    origin display name when it's an `msl://…` id, or from the
///    detected model name for plain-short-name matches) and render
///    the SVG if present.
/// 2. Class with an inline `Icon` annotation: TBD — needs an Icon-
///    primitives renderer. Currently shows the placeholder.
/// 3. Everything else: a friendly "no icon defined" placeholder so
///    the tab doesn't appear broken.
///
/// Always centred in the available rect, aspect-preserving.
/// Render the active class's `Documentation(info="…", revisions="…")`
/// annotation. HTML is shown raw — no Markdown conversion, no tag
/// stripping. Most Modelica docs are short prose with light HTML
/// (paragraphs, the occasional `<strong>` or `<code>`); the tags
/// read fine inline for a workbench built for engineers. Upgrading
/// to a Markdown-converted render is a follow-up.
fn render_docs_view(ui: &mut egui::Ui, world: &mut World) {
    
    let (heading_color, subtitle_color) = world
        .get_resource::<lunco_theme::Theme>()
        .map(|t| (t.tokens.text, t.tokens.text_subdued))
        .unwrap_or((
            egui::Color32::from_rgb(230, 235, 245),
            egui::Color32::from_rgb(170, 180, 195),
        ));
    let doc_id = world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document);
    if doc_id.is_none() {
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new("No model open").weak());
        });
        return;
    }

    // Resolve the class: drill-in target (qualified), or first non-
    // package class in the AST as fallback. Same picker the canvas's
    // target resolver uses.
    let (class_name, class_description, info, revisions): (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = match doc_id {
        None => (None, None, None, None),
        Some(doc) => {
            // B.3: derive from `ModelTabs`.
            let drilled = drilled_class_for_doc(world, doc);
            // All four fields read from the per-doc Index. Description
            // and Documentation(info=, revisions=) are pre-extracted
            // during rebuild — no AST walk per render.
            world
                .resource::<crate::ui::state::ModelicaDocumentRegistry>()
                .host(doc)
                .and_then(|h| {
                    let index = h.document().index();
                    let entry = if let Some(q) = drilled.as_deref() {
                        index.classes.get(q)
                    } else {
                        index
                            .classes
                            .values()
                            .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                            .or_else(|| index.classes.values().next())
                    }?;
                    let desc = if entry.description.is_empty() {
                        None
                    } else {
                        Some(entry.description.clone())
                    };
                    let (info, revs) = entry.documentation.clone();
                    Some((Some(entry.name.clone()), desc, info, revs))
                })
                .unwrap_or((None, None, None, None))
        }
    };

    // Typography: constrain reading width and centre in the panel.
    // Modelica docs open in whatever width the panel is — often
    // 1000+ px, which drops text line-length to 140+ characters. The
    // eye can't scan that; standard book / web typography caps at
    // ~65–75 characters (≈ 720 px at 13 px body), matching MDN, Rust
    // docs, and Obsidian's reading view.
    const READING_WIDTH: f32 = 760.0;
    const SIDE_MARGIN: f32 = 24.0;

    egui::ScrollArea::vertical()
        .auto_shrink([false; 2])
        .show(ui, |ui| {
            let avail = ui.available_width();
            let target_width = READING_WIDTH.min(avail - SIDE_MARGIN * 2.0);
            let inset = ((avail - target_width) * 0.5).max(SIDE_MARGIN);

            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(inset as i8, 16))
                .show(ui, |ui| {
                    ui.set_max_width(target_width);

                    if let Some(name) = &class_name {
                        ui.label(
                            egui::RichText::new(name)
                                .size(22.0)
                                .strong()
                                .color(heading_color),
                        );
                        // Class docstring as subtitle when present —
                        // Modelica's `model Foo "this is the doc"`
                        // and the next-line variant for packages
                        // (`package Foo` ↵ `  "doc"`). Distinct from
                        // the `Documentation(info=…)` HTML annotation
                        // rendered below; many MSL packages have only
                        // the docstring (no HTML), so without this
                        // the page header reads bare for them.
                        if let Some(desc) = &class_description {
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(desc)
                                    .size(13.0)
                                    .italics()
                                    .color(subtitle_color),
                            );
                        }
                        ui.add_space(12.0);
                    }
                    match info.as_deref().filter(|s| !s.trim().is_empty()) {
                        Some(html) => {
                            render_html_as_markdown(ui, world, target_width, html);
                        }
                        None => {
                            ui.label(
                                egui::RichText::new("(no documentation)")
                                    .italics()
                                    .weak(),
                            );
                        }
                    }
                    if let Some(revs) =
                        revisions.as_deref().filter(|s| !s.trim().is_empty())
                    {
                        ui.add_space(24.0);
                        ui.separator();
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("Revisions")
                                .strong()
                                .size(15.0)
                                .color(subtitle_color),
                        );
                        ui.add_space(6.0);
                        render_html_as_markdown(ui, world, target_width, revs);
                    }
                });
        });
}

/// Convert a Modelica-documentation HTML blob into Markdown with
/// [`htmd`] and render it via [`egui_commonmark::CommonMarkViewer`].
///
/// `target_width` is the reading-width cap applied to images so a
/// full-res MSL plot (often 1200+ px) doesn't blow past the column
/// and force the reader to scroll sideways. Keeping the Markdown
/// render cache static means repeated frames don't re-tokenise the
/// same text.
fn render_html_as_markdown(
    ui: &mut egui::Ui,
    world: &mut World,
    target_width: f32,
    html: &str,
) {
    use std::sync::Mutex;
    static CACHE: std::sync::OnceLock<
        Mutex<egui_commonmark::CommonMarkCache>,
    > = std::sync::OnceLock::new();
    let cache = CACHE
        .get_or_init(|| Mutex::new(egui_commonmark::CommonMarkCache::default()));

    // Memoise the HTML→Markdown conversion by input hash. `htmd`
    // is pure CPU and sub-ms for typical MSL docs, but the Docs
    // view re-calls us every frame while the tab is active — at
    // 60 fps a 2ms conversion is ~120ms/sec of main-thread work
    // for no reason. A single-entry cache is enough because the
    // same HTML is requested many frames in a row; switching
    // tabs changes the input, which re-converts once.
    static MD_CACHE: std::sync::OnceLock<
        Mutex<Option<(u64, String)>>,
    > = std::sync::OnceLock::new();
    let md_cache = MD_CACHE.get_or_init(|| Mutex::new(None));
    let html_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        html.hash(&mut h);
        h.finish()
    };
    let md = {
        let hit = md_cache
            .lock()
            .ok()
            .and_then(|g| g.as_ref().filter(|(k, _)| *k == html_hash).map(|(_, v)| v.clone()));
        match hit {
            Some(v) => v,
            None => {
                let v = htmd::convert(html).unwrap_or_else(|_| html.to_string());
                if let Ok(mut g) = md_cache.lock() {
                    *g = Some((html_hash, v.clone()));
                }
                v
            }
        }
    };
    if let Ok(mut c) = cache.lock() {
        egui_commonmark::CommonMarkViewer::new()
            .max_image_width(Some(target_width as usize))
            .show(ui, &mut c, &md);
    }

    // Intercept custom-scheme link clicks. `egui_commonmark` renders
    // markdown links as `ui.link`s that push an
    // `OutputCommand::OpenUrl(url)` into `PlatformOutput::commands`
    // when clicked — the OS-open flow. For schemes the workbench's
    // `UriRegistry` knows about, we take over: dispatch through the
    // registry, fire a `UriClicked` event for domain observers
    // (e.g. `modelica://` → OpenClass), and strip the command from
    // the output vec so the OS doesn't try to hand it to a browser
    // that wouldn't know what to do with it. http/https/mailto pass
    // through untouched — `NotHandled` leaves them in place.
    let intercepts: Vec<(usize, String, lunco_workbench::UriResolution)> = {
        let registry = world.get_resource::<lunco_workbench::UriRegistry>();
        ui.ctx().output_mut(|o| {
            let mut out = Vec::new();
            for (idx, cmd) in o.commands.iter().enumerate() {
                if let egui::OutputCommand::OpenUrl(open) = cmd {
                    let resolution = registry
                        .map(|r| r.dispatch(&open.url))
                        .unwrap_or(lunco_workbench::UriResolution::NotHandled);
                    if !matches!(
                        resolution,
                        lunco_workbench::UriResolution::NotHandled
                    ) {
                        out.push((idx, open.url.clone(), resolution));
                    }
                }
            }
            out
        })
    };
    if intercepts.is_empty() {
        return;
    }
    // Drop the intercepted commands back-to-front so earlier indices
    // stay valid. At Documentation-rendering scale (a handful of
    // commands per frame, almost never >1 link click per frame) the
    // cost is negligible.
    ui.ctx().output_mut(|o| {
        for (idx, _, _) in intercepts.iter().rev() {
            if *idx < o.commands.len() {
                o.commands.remove(*idx);
            }
        }
    });
    for (_, url, resolution) in intercepts {
        world.commands().trigger(lunco_workbench::UriClicked {
            uri: url,
            resolution,
        });
    }
}


/// Un-escape a Modelica string literal's body per MLS §2.4.6. The
/// subset we handle covers what Documentation HTML actually uses:
///   `\"`  → `"`    (attribute quotes)
///   `\\`  → `\`    (literal backslash)
///   `\n`  → LF     (line break)
///   `\t`  → tab
///   `\r`  → CR
/// Unknown `\x` sequences fall through as two chars so we don't
/// accidentally destroy source that htmd or commonmark might still
/// handle gracefully.
fn unescape_modelica_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                match n {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '\'' => out.push('\''),
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else {
                out.push('\\');
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Extract `Documentation(info="…", revisions="…")` — both are HTML
/// string payloads. Returns `(info, revisions)`.
/// Extract `(info, revisions)` HTML/text strings from a class's
/// `Documentation(info="...", revisions="...")` annotation.
/// Pulled out as `pub` so the per-doc Index can populate
/// [`crate::index::ClassEntry::documentation`] during rebuild.
pub fn extract_documentation(
    annotations: &[rumoca_session::parsing::ast::Expression],
) -> (Option<String>, Option<String>) {
    use rumoca_session::parsing::ast::{Expression, TerminalType};
    // Find the Documentation(...) call.
    let call = annotations.iter().find(|e| match e {
        Expression::FunctionCall { comp, .. } | Expression::ClassModification { target: comp, .. } => {
            comp.parts
                .first()
                .map(|p| p.ident.text.as_ref() == "Documentation")
                .unwrap_or(false)
        }
        _ => false,
    });
    let Some(call) = call else { return (None, None) };
    let args: &[Expression] = match call {
        Expression::FunctionCall { args, .. } => args.as_slice(),
        Expression::ClassModification { modifications, .. } => modifications.as_slice(),
        _ => return (None, None),
    };
    let str_arg = |name: &str| -> Option<String> {
        for a in args {
            let (arg_name, value) = match a {
                Expression::NamedArgument { name, value } => {
                    (name.text.as_ref(), value.as_ref())
                }
                Expression::Modification { target, value } => (
                    target.parts.first().map(|p| p.ident.text.as_ref()).unwrap_or(""),
                    value.as_ref(),
                ),
                _ => continue,
            };
            if arg_name != name {
                continue;
            }
            if let Expression::Terminal { terminal_type: TerminalType::String, token } = value {
                // Rumoca keeps the raw source slice on the token, which
                // still includes the surrounding `"…"` *and* the
                // Modelica-spec escape sequences (`\"` for a literal
                // quote, `\\` for a backslash, `\n` for a newline). For
                // Documentation HTML the `\"` attribute-quotes are the
                // loudest — un-escaping turns `<img src=\"…\"/>` back
                // into the literal `<img src="…"/>` so htmd + the
                // renderer see proper HTML.
                let raw = token.text.as_ref();
                let inner = raw.trim_start_matches('"').trim_end_matches('"');
                return Some(unescape_modelica_string(inner));
            }
        }
        None
    };
    (str_arg("info"), str_arg("revisions"))
}

fn render_icon_view(ui: &mut egui::Ui, world: &mut World) {
    let theme = world
        .get_resource::<lunco_theme::Theme>()
        .cloned()
        .unwrap_or_else(lunco_theme::Theme::dark);
    // Derive name + source from the registry directly.
    let (qualified, _source) = {
        let active = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document);
        let Some(doc) = active else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("No model open").weak());
            });
            return;
        };
        let registry = world.resource::<ModelicaDocumentRegistry>();
        let Some(host) = registry.host(doc) else {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("No model open").weak());
            });
            return;
        };
        let document = host.document();
        // `model_path` no longer exists outside the cache. Use the
        // origin's display name + canonical_path heuristic to
        // reconstruct the `msl://` prefix when applicable.
        let display = document.origin().display_name().to_string();
        let from_path = display
            .strip_prefix("msl://")
            .map(|s| s.to_string());
        let short = document
            .strict_ast()
            .and_then(|ast| crate::ast_extract::extract_model_name_from_ast(&ast))
            .unwrap_or_default();
        let source = document.source().to_string();
        (
            from_path.unwrap_or_else(|| short.clone()),
            std::sync::Arc::<str>::from(source),
        )
    };

    let painter = ui.painter();
    let rect = ui.available_rect_before_wrap();

    let frame_stroke_src = theme.colors.overlay1;
    painter.rect_stroke(
        rect.shrink(12.0),
        4.0,
        egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(
                frame_stroke_src.r(),
                frame_stroke_src.g(),
                frame_stroke_src.b(),
                120,
            ),
        ),
        egui::StrokeKind::Outside,
    );

    // Decoded `Icon(graphics={...})` from the class's own AST,
    // merged across the `extends` chain. The single source of truth
    // for the Icon view since the SVG fallback was retired.
    // (Icon, Parameters) — extracted together so duplicated /
    // user-authored classes that aren't in the MSL palette (e.g.
    // `InertiaCopy`) still get `%paramName` substitution.
    let (authored_icon, parameters) = {
        // Build the qualified class context for engine queries. The
        // engine resolves cross-file inheritance via rumoca's session
        // — no resolver lambda, no local-AST walk — so we just hand
        // it the fully-qualified name and read back merged Icon +
        // typed members. The within-prefixing logic is preserved so
        // bare class names (single-file workspace docs without
        // `within`) still resolve.
        let registry = world.resource::<ModelicaDocumentRegistry>();
        let class_context: Option<String> = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document)
            .and_then(|doc| registry.host(doc))
            .and_then(|host| {
                let document = host.document();
                if qualified.contains('.') {
                    return Some(qualified.clone());
                }
                let ast = document.strict_ast()?;
                let short = qualified.as_str();
                let pkg = ast
                    .within
                    .as_ref()
                    .map(|w| {
                        w.name
                            .iter()
                            .map(|t| t.text.as_ref())
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .unwrap_or_default();
                if pkg.is_empty() {
                    Some(short.to_string())
                } else {
                    Some(format!("{pkg}.{short}"))
                }
            });
        match (class_context, world.get_resource::<crate::engine_resource::ModelicaEngineHandle>()) {
            (Some(qpath), Some(handle)) => {
                let mut engine = handle.lock();
                let icon = crate::annotations::extract_icon_via_engine(&qpath, &mut engine);
                let parameters: Vec<(String, String)> = engine
                    .inherited_members_typed(&qpath)
                    .into_iter()
                    .filter(|m| {
                        matches!(
                            m.variability,
                            crate::engine::InheritedVariability::Parameter
                        )
                    })
                    .map(|m| {
                        let default = m.default_value.unwrap_or_default();
                        (m.name, default)
                    })
                    .collect();
                (icon, parameters)
            }
            _ => (None, Vec::new()),
        }
    };

    if let Some(icon) = authored_icon {
        let side = (rect.width().min(rect.height()) * 0.6).max(100.0);
        let icon_rect = egui::Rect::from_center_size(
            rect.center(),
            egui::vec2(side, side),
        );
        let short_name = qualified
            .rsplit('.')
            .next()
            .unwrap_or(&qualified)
            .to_string();
        let sub = crate::icon_paint::TextSubstitution {
            // On the Icon tab there's no specific instance — show the
            // class name for `%name`. `%class` is unambiguously the
            // class itself. Matches what Dymola puts in the Icon view.
            name: Some(short_name.as_str()),
            class_name: Some(short_name.as_str()),
            parameters: (!parameters.is_empty()).then_some(parameters.as_slice()),
        };
        crate::icon_paint::paint_graphics_themed(
            painter,
            icon_rect,
            icon.coordinate_system,
            crate::icon_paint::IconOrientation::default(),
            Some(&sub),
            None,
            Some(&theme.modelica_icons),
            &icon.graphics,
        );
        // No workbench-side class label — the icon's authored
        // `Text(textString="%name")` already renders the class name
        // (substituted with the qualified name on the Icon view, per
        // OMEdit/Dymola behaviour). Drawing a label below duplicates
        // the title that appears at the top of every authored MSL
        // icon.
        return;
    }

    // Fallback placeholder — the class has no known icon. Same
    // centered-card pattern the empty-diagram overlay uses.
    use crate::ui::theme::ModelicaThemeExt;
    crate::ui::panels::placeholder::render_centered_card(
        ui,
        rect,
        egui::vec2(380.0, 170.0),
        &theme,
        |ui| {
            ui.label(
                egui::RichText::new("🎨")
                    .size(36.0)
                    .color(theme.text_muted()),
            );
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("No icon defined for this class")
                    .strong()
                    .color(theme.text_heading()),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Add an `annotation(Icon(graphics={…}))` clause \
                     in the Text tab, or instantiate this class in a \
                     parent diagram.",
                )
                .italics()
                .size(11.0)
                .color(theme.text_muted()),
            );
        },
    );
}

