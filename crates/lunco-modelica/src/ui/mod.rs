//! Modelica workbench UI — panels as entity viewers.
//!
//! ## Architecture: Panels Are Entity Viewers
//!
//! Each panel watches a `ModelicaModel` entity and renders its data.
//! Panels don't know if they're in a standalone workbench, a floating overlay
//! on a 3D viewport, or a mission dashboard — they just watch the selected entity.
//!
//! ```text
//!                    ModelicaModel entity
//!                    (attached to 3D objects
//!                     or standalone workbench)
//!                              │
//!           ┌──────────────────┼──────────────────┐
//!           ▼                  ▼                  ▼
//!     DiagramPanel      CodeEditorPanel    TelemetryPanel
//!     (lunco-canvas)    (text editor)      (params/inputs)
//! ```
//!
//! ## Selection Bridge
//!
//! `WorkbenchState.selected_entity` is the single source of truth.
//! Any context can trigger an editor by setting it:
//! - Package Browser: click a model in the tree
//! - 3D viewport: click a rover's solar panel
//! - Colony tree: select a subsystem node
//!
//! ```rust,ignore
//! // Anywhere in the codebase:
//! fn open_modelica_editor(world: &mut World, entity: Entity) {
//!     if let Some(mut state) = world.get_resource_mut::<WorkbenchState>() {
//!         state.selected_entity = Some(entity);
//!     }
//!     // Panels auto-update because they watch WorkbenchState
//! }
//! ```
//!
//! ## Panel Layout
//!
//! bevy_workbench auto-assigns panel slots by ID convention:
//!
//! | ID Pattern         | Auto-Slot | Default Position  |
//! |--------------------|-----------|-------------------|
//! | contains "inspector" | Right   | Right dock        |
//! | contains "console"   | Bottom  | Bottom dock       |
//! | contains "preview"   | Center  | Center tab        |
//! | (no match)           | Left    | Left dock         |
//!
//! Users can drag, split, tab, and float panels freely.
//! Layout persists across sessions via bevy_workbench persistence.
//!
//! ## Panels
//!
//! - **Package Browser** (left dock) — Dymola-style library tree, click to open
//! - **Code Editor** (center tab) — source code editing, compile & run
//! - **Diagram** (center tab) — component block diagram on `lunco-canvas`
//! - **Telemetry** (right dock) — parameters, inputs, variable toggles
//! - **Graphs** (bottom dock) — time-series plots of simulation variables

use bevy::prelude::*;
use lunco_workbench::{Perspective, PerspectiveId, WorkbenchAppExt, WorkbenchLayout, PanelId};

pub mod state;
pub use state::*;

pub mod commands;
pub use commands::{CompileModel, CreateNewScratchModel, ModelicaCommandsPlugin};

pub mod icon_paint;
pub mod image_loader;
pub mod panels;
pub mod viz;
pub mod theme;
pub mod uri_handler;
pub mod loaded_classes;
pub mod welcome_progress;
/// Debounced AST reparse driver — see module docs.
pub mod ast_refresh;
pub mod input_activity;
/// Phase 1: bevy_vello-backed diagram canvas, one render target per
/// open document tab. See module docs.
pub mod vello_canvas;
// Renderer trait + backends moved to `lunco-canvas`. Re-export at
// the workbench level for callers that already pulled it in via
// `crate::ui::renderer::*` so the old import paths keep working
// during the migration.
pub use lunco_canvas::renderer;

/// Modelica section of the Twin Browser — class-tree contributed by
/// this crate to `lunco-workbench`'s `BrowserSectionRegistry`.
pub mod browser_section;

/// Drains the workbench's `BrowserActions` outbox and routes
/// section-emitted intents (open file, open Modelica class) into the
/// existing document-load and drill-in pipelines.
pub mod browser_dispatch;

use crate::ModelicaModel;

/// Fan queued document lifecycle notifications out as observer triggers.
///
/// The registry accumulates ids on every mutation (allocate → Opened +
/// Changed, `checkpoint_source` with new text → Changed, explicit
/// `mark_changed` after `host_mut` undo/redo → Changed, `remove_document`
/// → Closed). This system drains all three queues once per frame and
/// emits the matching generic events from [`lunco_doc_bevy`] so any
/// observer (panel re-render, diagram re-parse, plot variable-list
/// refresh, Twin journal, …) reacts without polling generation
/// counters.
///
/// Fire order per frame: Opened, Changed, Closed. Opened-before-Changed
/// means subscribers that key on "track docs I've seen Opened for" can
/// safely skip Changed events for unknown ids.
fn drain_document_changes(
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut commands: Commands,
) {
    for doc in registry.drain_pending_opened() {
        commands.trigger(lunco_doc_bevy::DocumentOpened::local(doc));
    }
    for doc in registry.drain_pending_changes() {
        commands.trigger(lunco_doc_bevy::DocumentChanged::local(doc));
    }
    for doc in registry.drain_pending_closed() {
        commands.trigger(lunco_doc_bevy::DocumentClosed::local(doc));
    }
}

/// Shadow-sync observer: a Modelica doc was added → if it's writable
/// (User-saved) or Untitled (in-memory draft), register it as a
/// top-level [`LoadedClass`](crate::ui::loaded_classes::LoadedClass)
/// in the Twin panel. Read-only library reference docs (MSL classes
/// the user clicked through to inspect) skip — they're already
/// reachable through the system-library trees.
fn register_workspace_class_on_doc_opened(
    trigger: On<lunco_doc_bevy::DocumentOpened>,
    registry: Res<crate::ui::state::ModelicaDocumentRegistry>,
    mut loaded: ResMut<loaded_classes::LoadedModelicaClasses>,
) {
    let doc_id = trigger.event().doc;
    let Some(host) = registry.host(doc_id) else {
        return;
    };
    let origin = host.document().origin();
    if !(origin.is_writable() || origin.is_untitled()) {
        return;
    }
    // Dedupe — DocumentOpened can fire several times per doc id
    // during the open-pipeline races.
    let new_id = format!("workspace:{}", doc_id.raw());
    if loaded.entries.iter().any(|c| c.id() == new_id) {
        return;
    }
    loaded.register(Box::new(loaded_classes::WorkspaceClass::new(doc_id)));
}

/// Shadow-sync observer: a Modelica doc closed → drop its
/// `WorkspaceClass` entry.
fn drop_workspace_class_on_doc_closed(
    trigger: On<lunco_doc_bevy::DocumentClosed>,
    mut loaded: ResMut<loaded_classes::LoadedModelicaClasses>,
) {
    let doc_id = trigger.event().doc;
    let id = format!("workspace:{}", doc_id.raw());
    loaded.unregister(&id);
}

/// Shadow-sync observer: Modelica doc opened → register entry in the
/// Workspace session.
///
/// Runs alongside (not instead of) the existing open paths during the
/// 5b.1 migration. Once step 5c retires the legacy `ModelicaDocumentRegistry`
/// / `ModelTabs` / `WorkbenchState.open_model` triad, this observer
/// becomes the sole population point for the Workspace's document list.
/// Observer: a Modelica doc was mutated → re-mirror the post-edit
/// source into `WorkbenchState::open_model` so the code editor (which
/// reads `open_model.source`, not the registry) shows the change
/// immediately. Covers every edit path uniformly: SetDocumentSource,
/// Add/RemoveComponent, Connect/Disconnect, undo/redo, canvas drag,
/// scripted batches.
fn mirror_open_model_on_doc_changed(
    trigger: On<lunco_doc_bevy::DocumentChanged>,
    registry: Res<ModelicaDocumentRegistry>,
    workspace: Res<lunco_workbench::WorkspaceResource>,
    mut state: ResMut<crate::ui::state::WorkbenchState>,
) {
    let doc = trigger.event().doc;
    // Only mirror when the active doc changed — `open_model` tracks
    // the foreground doc only. A background-doc edit (rare today, but
    // possible via API) shouldn't displace what the user is editing.
    if workspace.active_document != Some(doc) {
        return;
    }
    let Some(host) = registry.host(doc) else { return };
    let Some(open) = state.open_model.as_mut() else { return };
    let src = host.document().source();
    let mut line_starts = vec![0usize];
    for (i, b) in src.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            line_starts.push(i + 1);
        }
    }
    open.source = std::sync::Arc::from(src);
    open.line_starts = line_starts.into();
    open.cached_galley = None;
}

fn sync_workspace_on_doc_opened(
    trigger: On<lunco_doc_bevy::DocumentOpened>,
    registry: Res<ModelicaDocumentRegistry>,
    mut ws: ResMut<lunco_workbench::WorkspaceResource>,
) {
    let id = trigger.event().doc;
    // Dedupe — `DocumentOpened` can fire multiple times per id during
    // the race between allocate/install_prebuilt and later reconcile
    // passes. Treat a second Opened as a no-op so the Workspace
    // document list stays a set, not a multiset.
    if ws.document(id).is_some() {
        return;
    }
    let Some(host) = registry.host(id) else {
        return;
    };
    let doc = host.document();
    let origin = doc.origin().clone();
    let title = match &origin {
        lunco_doc::DocumentOrigin::Untitled { name } => name.clone(),
        lunco_doc::DocumentOrigin::File { path, .. } => path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(file)")
            .to_string(),
    };
    ws.add_document(lunco_workspace::DocumentEntry {
        id,
        kind: lunco_workspace::DocumentKind::Modelica,
        origin,
        // Default to `None`; when the UI supports "New Model from
        // active Twin" the caller will set this explicitly before the
        // add_document fires.
        context_twin: None,
        title,
    });
}

/// Shadow-sync observer: Modelica doc closed → drop entry from Workspace.
fn sync_workspace_on_doc_closed(
    trigger: On<lunco_doc_bevy::DocumentClosed>,
    mut ws: ResMut<lunco_workbench::WorkspaceResource>,
) {
    ws.close_document(trigger.event().doc);
}

/// Shadow-sync observer: a save (regular or Save-As) can change a
/// document's origin (Untitled → File on Save-As). Re-read the
/// document and update the Workspace entry's `origin` + `title`.
///
/// `DocumentSaved` fires for every save, not only Save-As; the update
/// is idempotent for regular Save (origin unchanged, title unchanged)
/// so no gate is needed.
fn sync_workspace_on_doc_saved(
    trigger: On<lunco_doc_bevy::DocumentSaved>,
    registry: Res<ModelicaDocumentRegistry>,
    mut ws: ResMut<lunco_workbench::WorkspaceResource>,
) {
    let id = trigger.event().doc;
    let Some(host) = registry.host(id) else { return };
    let doc = host.document();
    let new_origin = doc.origin().clone();
    let new_title = match &new_origin {
        lunco_doc::DocumentOrigin::Untitled { name } => name.clone(),
        lunco_doc::DocumentOrigin::File { path, .. } => path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("(file)")
            .to_string(),
    };
    // Push to recents on every File-saved event. `push_loose` dedupes
    // to the front, so re-saving an existing file simply hoists it to
    // the top — matches VS Code behaviour and is what makes Save-As
    // of an Untitled draft show up in "Open Recent File" next session
    // (the rebind from Untitled → File doesn't otherwise re-trigger
    // `add_document`, which is the only other recents push site).
    if let Some(p) = new_origin.canonical_path() {
        ws.recents.push_loose(p.to_path_buf());
    }
    if let Some(entry) = ws.document_mut(id) {
        entry.origin = new_origin;
        entry.title = new_title;
    }
}

/// Derive `WorkspaceResource.DocumentEntry.title` from the AST's
/// first top-level class name. Modelica's class-first identity model
/// (Dymola / OMEdit) means the tab label should follow the class, not
/// the original Untitled-N or filename — see
/// `docs/architecture/20-domain-modelica.md` § 7a.
///
/// Fallback ladder: AST first-class name → `origin.display_name()`
/// (file stem or `Untitled-N`).
///
/// Untitled docs also get their `origin.name` rewritten to match the
/// class name, so subsequent Save-As prompts default to
/// `<class>.mo` and the Files browser groups consistently.
///
/// TODO(modelica.naming.tab_title_source) — make the choice between
/// "ClassName" (current behaviour) vs "FileName" (VS Code) settings-
/// driven. Today the rule is hardcoded to ClassName.
///
/// TODO(ui.italic_for_unsaved) — italic styling on the tab label is
/// the renderer's job (lunco-workbench tab widget); not implemented
/// yet. Dirty-dot `●` likewise.
///
/// TODO(multi-class breadcrumb) — for `package P; model A; model B; end P;`
/// docs, this currently shows `P` (the first top-level class). Once
/// drilled-in tracking is per-doc-tab (it's per-canvas today), the
/// derived title should become `P.<drilled>` to match Dymola.
fn derive_doc_title(
    registry: Res<ModelicaDocumentRegistry>,
    mut ws: ResMut<lunco_workbench::WorkspaceResource>,
) {
    // Cheap when nothing changed: each iteration is a HashMap lookup +
    // a string compare, write only on diff. No per-doc generation
    // tracking yet — add one if profiling shows this in a hot frame.
    for (doc_id, host) in registry.docs() {
        let document = host.document();
        let derived = derive_title_from_doc(document);
        let Some(entry) = ws.document_mut(doc_id) else {
            continue;
        };
        if entry.title != derived {
            entry.title = derived.clone();
        }
        // For Untitled docs, also keep the origin in sync so Save-As
        // suggestions and other origin-readers see the new identity.
        if let lunco_doc::DocumentOrigin::Untitled { name } = &entry.origin {
            if name.as_str() != derived.as_str() {
                entry.origin = lunco_doc::DocumentOrigin::untitled(derived);
            }
        }
    }
}

/// Pure helper: read the first AST class name out of a Modelica doc,
/// fall back to the origin's display name. Kept separate so future
/// drilled-in / multi-class logic plugs in without re-deriving the
/// fallback chain.
fn derive_title_from_doc(doc: &crate::document::ModelicaDocument) -> String {
    let syntax = doc.syntax();
    if let Some((name, _)) = syntax.ast().classes.iter().next() {
        if !name.is_empty() {
            return name.clone();
        }
    }
    doc.origin().display_name()
}

/// React to a Twin being added (Open Folder / Open Twin / promotion)
/// by spawning a background scan task that builds the package-browser
/// tree for that Twin's `.mo` content.
///
/// The scan was previously inlined into the welcome panel's "Open
/// Folder" button. Hoisting it onto the canonical `TwinAdded` event
/// means menu / picker / HTTP / scripts all converge on one path —
/// the welcome button is now just another fire-and-forget caller.
fn scan_twin_on_added(
    trigger: On<lunco_workbench::TwinAdded>,
    ws: Res<lunco_workbench::WorkspaceResource>,
    mut cache: ResMut<panels::package_browser::PackageTreeCache>,
) {
    let twin_id = trigger.event().twin;
    let Some(twin) = ws.twin(twin_id) else {
        return;
    };
    let folder = twin.root.clone();
    let pool = bevy::tasks::AsyncComputeTaskPool::get();
    let task = pool.spawn(async move { panels::package_browser::scan_twin_folder(folder) });
    cache.twin = None;
    cache.twin_scan_task = Some(task);
}

/// Drop the document linked to a despawned `ModelicaModel` entity, and
/// any compile-state bookkeeping attached to that document.
///
/// Behavior preserved from the entity-keyed era: when an entity is
/// despawned, its backing [`ModelicaDocument`](crate::document::ModelicaDocument)
/// is also removed. The long-term design lets documents outlive entities
/// (edit-without-running, cosim re-spawn), so this will become opt-in
/// once the tab/view layer can explicitly unload a document.
fn cleanup_removed_documents(
    mut removed: RemovedComponents<ModelicaModel>,
    registry: Option<ResMut<ModelicaDocumentRegistry>>,
    compile_states: Option<ResMut<CompileStates>>,
    canvas_state: Option<ResMut<panels::canvas_diagram::CanvasDiagramState>>,
    class_names: Option<ResMut<panels::canvas_diagram::DrilledInClassNames>>,
    signals: Option<ResMut<lunco_viz::SignalRegistry>>,
    viz_registry: Option<ResMut<lunco_viz::VisualizationRegistry>>,
) {
    let Some(mut registry) = registry else { return };
    let mut compile_states = compile_states;
    let mut canvas_state = canvas_state;
    let mut class_names = class_names;
    let mut signals = signals;
    let mut viz_registry = viz_registry;
    for entity in removed.read() {
        if let Some(doc) = registry.unlink_entity(entity) {
            registry.remove_document(doc);
            if let Some(states) = compile_states.as_mut() {
                states.remove(doc);
            }
            // Drop the per-doc canvas entry (viewport, selection,
            // in-flight projection task) so a later tab reusing the
            // id starts fresh. Matches how CompileStates is cleaned.
            if let Some(canvas) = canvas_state.as_mut() {
                canvas.drop_doc(doc);
            }
            if let Some(names) = class_names.as_mut() {
                names.remove(doc);
            }
        }
        // Drop every registered signal + plot binding for this entity
        // so stale plots don't keep reading the last values forever.
        if let Some(sigs) = signals.as_mut() {
            sigs.drop_entity(entity);
        }
        if let Some(reg) = viz_registry.as_mut() {
            crate::ui::viz::drop_entity_bindings(reg, entity);
        }
    }
}

/// The Modelica workbench's default workspace preset.
///
/// Mirrors the "Analyze — Modelica deep dive" slot map from the workbench
/// design doc ([`docs/architecture/11-workbench.md`] § 4).
pub struct AnalyzePerspective;

impl Perspective for AnalyzePerspective {
    fn id(&self) -> PerspectiveId { PerspectiveId("modelica_analyze") }
    fn title(&self) -> String { "📊 Analyze".into() }
    fn apply(&self, layout: &mut WorkbenchLayout) {
        layout.set_activity_bar(false);
        // Side dock = Twin Browser only. The legacy
        // `PackageBrowserPanel` stays registered (View → Panels can
        // re-dock it) but is not docked by default — its remaining
        // unique features (MSL palette, drag-to-instantiate) will
        // migrate into the Twin Browser as a future `MslSection`.
        // Side-by-side dock would just present users with two
        // browsers solving the same job.
        // Two sibling tabs in the side dock — Twin (everything you
        // browse by name: workspace classes, MSL, bundled, future
        // USD/SysML — matches Dymola/OMEdit's single-Package-Browser
        // pattern) and Files (raw FS). Twin is leftmost so it's the
        // default active tab on first launch.
        layout.set_side_browser_tabs(vec![
            lunco_workbench::TWIN_BROWSER_PANEL_ID,
            lunco_workbench::FILES_PANEL_ID,
        ]);
        // Center is seeded with no singleton tab — model views are
        // multi-instance tabs opened dynamically by the Package Browser
        // (one tab per open document). An app that boots with a
        // default model can pre-open a tab after setup via
        // `WorkbenchLayout::open_instance(MODEL_VIEW_KIND, doc.raw())`.
        //
        // Keep a placeholder center tab so the dock's cross layout
        // still builds on apps with nothing open yet. When the first
        // real model tab opens, the placeholder stays docked next
        // to it — users can close it.
        layout.set_center(vec![PanelId("modelica_welcome")]);
        layout.set_active_center_tab(0);
        // Right dock — Telemetry (parameters, inputs, variable
        // toggles), Inspector (selected node's modifications), and
        // Component Palette (MSL instantiation). The Telemetry panel
        // is registered under the historical id `modelica_inspector`
        // for layout-stability reasons; the new selection-driven
        // Inspector uses `modelica_diagram_inspector`.
        layout.set_right_inspector_tabs(vec![
            PanelId("modelica_inspector"),
            PanelId("modelica_diagram_inspector"),
            PanelId("modelica_component_palette"),
        ]);
        // Bottom dock: Graphs first so it's the default active tab —
        // the simulation plot is what a user running a model wants
        // to see on landing, not the log stream. Console stays one
        // click away for compile / save / error output (VS Code's
        // Terminal/Output/Problems pattern, just with a different
        // default active tab).
        layout.set_bottom_tabs(vec![
            PanelId("modelica_graphs"),
            PanelId("modelica_diagnostics"),
            PanelId("modelica_console"),
            PanelId("modelica_journal"),
        ]);
    }
}

/// Plugin that registers all Modelica workbench UI panels.
///
/// Panels are entity viewers — they watch `WorkbenchState.selected_entity`
/// and render data for the active `ModelicaModel`. They work in any context:
/// standalone workbench, 3D overlay, or mission dashboard.
pub struct ModelicaUiPlugin;

impl Plugin for ModelicaUiPlugin {
    fn build(&self, app: &mut App) {
        // Twin-level change journal subscribes to the generic document
        // lifecycle events this plugin fires. One journal per App —
        // adding the plugin multiple times is a no-op on `init_resource`.
        app.add_plugins(lunco_doc_bevy::TwinJournalPlugin);

        // Shared Modelica class cache — drill-in, preload, and
        // (later) compile dep-walk all funnel through this one
        // Arc-shared store so every .mo file is read once and
        // parsed once per session.
        app.add_plugins(crate::class_cache::ClassCachePlugin);

        // Intent layer: key chords → EditorIntent. Domain resolvers
        // (installed by ModelicaCommandsPlugin below) translate intents
        // into concrete commands for the docs they own.
        app.add_plugins(lunco_doc_bevy::EditorIntentPlugin);

        // Command bus for Modelica documents — Undo / Redo / Save /
        // Close (generic) + Compile (domain-specific) — plus the
        // EditorIntent resolver. UI buttons, keyboard shortcuts,
        // scripts, and the remote API all funnel through these.
        app.add_plugins(ModelicaCommandsPlugin);

        // Welcome-panel open-counter ledger. Loads the persisted
        // JSON at startup and bumps counts whenever `OpenClass`
        // fires — drives the progress dots on the learning paths.
        app.add_plugins(welcome_progress::WelcomeProgressPlugin);

        // Reflect-registered query providers exposed over the
        // ApiQueryRegistry (cf. spec 032). Feature-gated because the
        // registry only exists when `lunco-api` is enabled.
        #[cfg(feature = "lunco-api")]
        app.add_plugins(crate::api_queries::ModelicaApiQueriesPlugin);

        // Edit events — always registered so the GUI and tests can
        // dispatch them. External API exposure is gated separately
        // inside the plugin via `ApiVisibility` (off by default; pass
        // `--api-expose-edits` to expose). See
        // `crates/lunco-modelica/src/api_edits.rs` for the rationale.
        app.add_plugins(crate::api_edits::ModelicaApiEditPlugin);

        app.init_resource::<WorkbenchState>()
            .init_resource::<ModelicaDocumentRegistry>()
            .init_resource::<CompileStates>()
            .init_resource::<panels::model_view::ModelTabs>()
            .init_resource::<panels::code_editor::EditorBufferState>()
            .init_resource::<panels::console::ConsoleLog>()
            .init_resource::<panels::diagnostics::DiagnosticsLog>()
            .init_resource::<panels::journal::JournalLog>()
            .add_systems(Update, panels::journal::poll_changes)
            // Canvas animation: API-driven AddComponent calls queue a
            // pending camera focus; this system applies it via
            // `viewport.set_target` (which auto-eases) once the new
            // node has landed in the projected scene. See
            // `docs/architecture/20-domain-modelica.md` § 9c.
            .init_resource::<panels::canvas_diagram::PendingApiFocusQueue>()
            .init_resource::<panels::canvas_diagram::PendingApiConnectionQueue>()
            .init_resource::<panels::canvas_diagram::CinematicCamera>()
            .add_systems(
                Update,
                (
                    panels::canvas_diagram::drive_pending_api_focus,
                    // Tick must run AFTER the focus driver so a move
                    // queued this frame gets sampled the same frame
                    // it's planned (saves one frame of "stuck" feel).
                    panels::canvas_diagram::tick_cinematic_camera,
                    // Connections fire alongside node adds and use
                    // the same `EdgePulseLayer` to flash; ordering
                    // doesn't matter relative to the camera tick.
                    panels::canvas_diagram::drive_pending_api_connections,
                )
                    .chain(),
            )
            // Forward StatusBus events to the Console panel so the
            // user has a chronological audit trail of every status
            // event from every subsystem (MSL, compile, sim, …).
            .add_systems(Update, fan_status_bus_to_console)
            .init_resource::<panels::canvas_projection::DiagramAutoLayoutSettings>()
            .init_resource::<panels::palette::PaletteState>()
            .init_resource::<panels::palette::ComponentDragPayload>()
            .insert_resource(panels::package_browser::PackageTreeCache::new())
            .init_resource::<browser_dispatch::PendingDrillIns>()
            .add_systems(Update, browser_dispatch::drain_browser_actions)
            .add_systems(Update, panels::package_browser::handle_package_loading_tasks)
            .add_systems(Update, cleanup_removed_documents)
            .add_systems(Update, drain_document_changes)
            // Workspace shadow-sync: keep `WorkspaceResource` populated
            // from the existing document-registry lifecycle so the new
            // session surface is ready for step 5b.2 readers.
            .add_observer(sync_workspace_on_doc_opened)
            .add_observer(sync_workspace_on_doc_closed)
            .add_observer(sync_workspace_on_doc_saved)
            // Mirror the post-edit source from the registry into
            // `WorkbenchState::open_model` whenever any mutation lands —
            // structured ops via API, canvas drag, code-editor keystroke.
            // The code editor reads `open_model.source` (Arc<str>), not
            // the registry directly, so without this fan-out
            // SetDocumentSource / Add* / Connect* edits update the
            // canvas (which reads the registry) but leave the text
            // editor stuck on the old source.
            .add_observer(mirror_open_model_on_doc_changed)
            .add_systems(Update, derive_doc_title)
            // Twin-panel: keep the loaded-classes list in sync with
            // the document registry. One `WorkspaceClass` per
            // writable / Untitled Modelica doc, dropped on close.
            .add_observer(register_workspace_class_on_doc_opened)
            .add_observer(drop_workspace_class_on_doc_closed)
            // Kick off a background scan whenever the workbench
            // announces a new Twin (Open Folder / Open Twin / "Save
            // as Twin" promotion). The scan populates the package
            // browser's Twin tree; until this lands, opening a Twin
            // would update WorkspaceResource but the Modelica
            // sidebar wouldn't reflect it.
            .add_observer(scan_twin_on_added)
            .add_systems(Update, panels::diagnostics::refresh_diagnostics)
            // Debounced AST reparse — reparses any doc that has
            // stopped receiving keystrokes for AST_DEBOUNCE_MS (250 ms).
            // Keeps text-edit latency constant regardless of how busy
            // the sim worker is.
            .init_resource::<ast_refresh::PendingAstParses>()
            .init_resource::<input_activity::InputActivity>()
            .add_systems(bevy::prelude::PreUpdate, input_activity::stamp_user_input)
            .add_systems(Update, ast_refresh::refresh_stale_asts)
            .add_systems(Startup, register_settings_menu)
            // Image-loader install is a first-frame one-shot — runs
            // in the egui primary-context pass until the context is
            // ready and the loaders land, then the marker resource
            // `ImageLoadersInstalled` short-circuits the run_if and
            // Bevy stops calling us entirely.
            .add_systems(
                bevy_egui::EguiPrimaryContextPass,
                install_image_loaders_once.run_if(
                    bevy::ecs::schedule::common_conditions::not(
                        bevy::ecs::schedule::common_conditions::resource_exists::<
                            ImageLoadersInstalled,
                        >,
                    ),
                ),
            )
            .register_panel(panels::package_browser::PackageBrowserPanel)
            .register_panel(lunco_workbench::TwinBrowserPanel)
            .register_panel(lunco_workbench::FilesPanel)
            .register_panel(panels::welcome::WelcomePanel)
            .register_panel(panels::telemetry::TelemetryPanel)
            .register_panel(panels::graphs::GraphsPanel)
            .register_panel(panels::console::ConsolePanel)
            .register_panel(panels::diagnostics::DiagnosticsPanel)
            .register_panel(panels::journal::JournalPanel)
            .register_panel(panels::canvas_diagram::CanvasDiagramPanel)
            .init_resource::<panels::canvas_diagram::CanvasDiagramState>()
            .init_resource::<panels::canvas_diagram::PaletteSettings>()
            .init_resource::<panels::canvas_diagram::DiagramProjectionLimits>()
            .init_resource::<panels::canvas_diagram::DrilledInClassNames>()
            .init_resource::<panels::canvas_diagram::DrillInLoads>()
            .init_resource::<panels::canvas_diagram::CanvasSnapSettings>()
            .init_resource::<panels::canvas_diagram::DuplicateLoads>()
            .add_systems(Update, panels::canvas_diagram::drive_drill_in_loads)
            .add_systems(Update, panels::canvas_diagram::drive_duplicate_loads)
            .register_panel(panels::inspector::InspectorPanel)
            .register_panel(panels::palette::ComponentPalettePanel)
            // Multi-instance: one tab per open document. Instances are
            // opened at runtime by the Package Browser.
            .register_instance_panel(panels::model_view::ModelViewPanel::default())
            .register_perspective(AnalyzePerspective);

        // Contribute the Modelica section to the Twin Browser's
        // section registry. The workbench's WorkbenchPlugin already
        // installed the registry resource and the built-in Files
        // section; we just append. ensure it exists first to avoid
        // panics during mixed-mode or deferred plugin builds.
        app.init_resource::<lunco_workbench::BrowserSectionRegistry>();
        // One section per domain — `ModelicaSection` iterates a
        // live `LoadedModelicaClasses` registry. Each entry is one
        // top-level Modelica class (system library, twin.toml
        // external, workspace document, future remote source) —
        // OMEdit's flat-list-of-libraries shape. Future domain
        // crates (`UsdSection`, `SysmlSection`, ...) follow the
        // same outer pattern with their own per-domain registry.
        app.world_mut()
            .resource_mut::<lunco_workbench::BrowserSectionRegistry>()
            .register(browser_section::ModelicaSection::default());
        app.init_resource::<loaded_classes::LoadedModelicaClasses>();
        // Default-libraries set: always loaded, not bound to any
        // Twin. MSL is the foundation; Bundled Examples is
        // LunCoSim's own learning material. Future implicit libs
        // (ModelicaServices, Complex) register here too.
        let mut loaded = app
            .world_mut()
            .resource_mut::<loaded_classes::LoadedModelicaClasses>();
        loaded.register(Box::new(loaded_classes::SystemLibraryClass::new(
            "msl_root",
            "Modelica",
            false,
        )));
        loaded.register(Box::new(loaded_classes::SystemLibraryClass::new(
            "bundled_root",
            "LunCo Examples",
            false,
        )));
    }
}

/// Push Modelica editor preferences onto the application-wide
/// Settings menu. Lives in the workbench Settings dropdown rather
/// than a per-panel gear button — keeps editor toolbar tidy and
/// all prefs discoverable in one place.
fn register_settings_menu(world: &mut World) {
    use bevy_egui::egui;
    let Some(mut layout) = world
        .get_resource_mut::<lunco_workbench::WorkbenchLayout>()
    else {
        return;
    };
    layout.register_settings(|ui, world| {
        ui.label(egui::RichText::new("Code Editor").weak().small());
        let mut buf = world.resource_mut::<panels::code_editor::EditorBufferState>();
        ui.checkbox(&mut buf.word_wrap, "Word wrap")
            .on_hover_text("Wrap long lines at editor width");
        ui.checkbox(&mut buf.auto_indent, "Auto indent")
            .on_hover_text("Copy previous line's indent on Enter");
        drop(buf);
        ui.separator();
        ui.label(egui::RichText::new("Component Palette").weak().small());
        let mut palette =
            world.resource_mut::<panels::canvas_diagram::PaletteSettings>();
        ui.checkbox(
            &mut palette.show_icon_only_classes,
            "Show icon-only classes",
        )
        .on_hover_text(
            "Include decorative classes from `Modelica.*.Icons.*` \
             subpackages in the add-component menu. Off by default \
             because they have no connectors and typically aren't \
             what a user wants to drop on a diagram.",
        );
        drop(palette);
        ui.separator();
        ui.label(egui::RichText::new("Diagram").weak().small());
        let mut limits =
            world.resource_mut::<panels::canvas_diagram::DiagramProjectionLimits>();
        ui.horizontal(|ui| {
            ui.label("Max nodes");
            ui.add(
                egui::DragValue::new(&mut limits.max_nodes)
                    .range(10..=100_000)
                    .speed(10.0),
            )
            .on_hover_text(
                "Upper bound on component count before the projector \
                 bails out with a warning. Raise for large models; \
                 lower if projections feel slow on modest hardware.",
            );
        });
        ui.horizontal(|ui| {
            ui.label("Timeout (s)");
            let mut secs = limits.max_duration.as_secs();
            if ui
                .add(
                    egui::DragValue::new(&mut secs)
                        .range(1_u64..=3600)
                        .speed(1.0),
                )
                .on_hover_text(
                    "Wall-clock deadline for a single projection. \
                     If the background parse + build takes longer, \
                     the task is cancelled and the canvas stays empty \
                     with a log warning. Default 60 s — only huge or \
                     pathological models get close.",
                )
                .changed()
            {
                limits.max_duration = std::time::Duration::from_secs(secs);
            }
        });
        drop(limits);
        ui.add_space(4.0);
        // ── Drag snap ────────────────────────────────────────────
        // Off by default — a lot of Modelica source uses
        // hand-placed non-grid positions and the user shouldn't
        // have their authored placements auto-rounded unless they
        // opted in. When on, drags quantise *live* (visible during
        // the drag itself) to multiples of `step` Modelica units.
        let mut snap =
            world.resource_mut::<panels::canvas_diagram::CanvasSnapSettings>();
        ui.checkbox(&mut snap.enabled, "Snap to grid on drag").on_hover_text(
            "When on, dragging an icon quantises its position to a \
             grid. Applies live during the drag and at commit. Off \
             by default.",
        );
        ui.horizontal(|ui| {
            ui.label("Grid step");
            ui.add_enabled(
                snap.enabled,
                egui::DragValue::new(&mut snap.step)
                    .range(0.5..=50.0)
                    .speed(0.5)
                    .suffix(" units"),
            )
            .on_hover_text(
                "Snap granularity in Modelica diagram-coordinate \
                 units (the 200-unit standard system). Common: 2 \
                 (fine), 5 (medium), 10 (coarse).",
            );
        });
        drop(snap);
    });
}

/// Marker resource — inserted by
/// [`install_image_loaders_once`] once the egui context is ready and
/// the loaders are wired. The system's `run_if(not(resource_exists))`
/// condition means Bevy stops scheduling the system after this
/// resource appears, so we pay exactly one successful install plus
/// however many frames we had to wait for the context to come up
/// (typically one or two).
#[derive(bevy::prelude::Resource)]
struct ImageLoadersInstalled;

/// First-frame egui image-loader registration. Gated by a `run_if`
/// so Bevy stops scheduling it after the first successful install —
/// no per-frame cost at all, not even a function-call return.
fn install_image_loaders_once(
    mut commands: bevy::prelude::Commands,
    mut contexts: bevy_egui::EguiContexts,
) {
    let Ok(ctx) = contexts.ctx_mut() else {
        // Context not ready yet — the run_if keeps scheduling us so
        // we get another shot next frame.
        return;
    };
    // Built-in loaders for file://, http(s)://, raw paths, bytes://,
    // etc. Covers everything the Modelica Documentation HTML can
    // reference through normal URIs.
    egui_extras::install_image_loaders(ctx);
    // Custom loader for `modelica://Package/Resources/…` URIs used
    // throughout MSL Documentation blocks.
    let loader = std::sync::Arc::new(image_loader::ModelicaImageLoader::new());
    ctx.add_bytes_loader(loader.clone());
    bevy::log::info!(
        "[ModelicaImageLoader] installed egui_extras loaders + modelica:// loader"
    );

    commands.insert_resource(ImageLoadersInstalled);
}

/// Forward newly-pushed [`lunco_workbench::status_bus::StatusBus`]
/// events to the [`panels::console::ConsoleLog`].
///
/// We track the count of *discrete* history entries we've already
/// mirrored so progress ticks (which mutate the bus seq but don't
/// append to history) don't show up as console spam. New entries
/// arrive at the back of the ring buffer; old ones drop off the front
/// when capacity is hit. We use a (last_seen_seq, last_back_message)
/// pair to detect "new entries since we last looked" without needing
/// per-event sequence numbers.
fn fan_status_bus_to_console(
    bus: bevy::prelude::Res<lunco_workbench::status_bus::StatusBus>,
    mut console: bevy::prelude::ResMut<panels::console::ConsoleLog>,
    mut last_count: bevy::prelude::Local<usize>,
) {
    let count = bus.history().count();
    if count == 0 {
        *last_count = 0;
        return;
    }
    if count == *last_count {
        return;
    }
    // The history ring buffer can lose entries from the front when
    // capacity hits. We only forward what's *new* at the back since
    // last we looked. Skip the first `(count - delta).min(count)`
    // events; forward the rest.
    let delta = count.saturating_sub(*last_count);
    for ev in bus.history().rev().take(delta).collect::<Vec<_>>().into_iter().rev() {
        let level = match ev.level {
            lunco_workbench::status_bus::StatusLevel::Info => panels::console::ConsoleLevel::Info,
            lunco_workbench::status_bus::StatusLevel::Warn => panels::console::ConsoleLevel::Warn,
            lunco_workbench::status_bus::StatusLevel::Error => panels::console::ConsoleLevel::Error,
            // Progress events shouldn't be in `history` (they live in
            // active_progress), but if one ever sneaks in, surface as Info.
            lunco_workbench::status_bus::StatusLevel::Progress => panels::console::ConsoleLevel::Info,
        };
        console.push(level, format!("[{}] {}", ev.source, ev.message));
    }
    *last_count = count;
}
