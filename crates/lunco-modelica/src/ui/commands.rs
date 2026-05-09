//! Command bus for Modelica documents.
//!
//! Every user intent that mutates a [`ModelicaDocument`] is a Bevy event
//! fired via `commands.trigger(...)`; the observers in this module are
//! the single write surface. UI buttons, keyboard shortcuts, the remote
//! API, and scripting all funnel through the same path.
//!
//! The generic commands ([`lunco_doc_bevy::UndoDocument`] /
//! [`RedoDocument`](lunco_doc_bevy::RedoDocument) /
//! [`SaveDocument`](lunco_doc_bevy::SaveDocument) /
//! [`CloseDocument`](lunco_doc_bevy::CloseDocument)) carry a
//! [`DocumentId`] without naming a domain. Each observer here checks
//! whether [`ModelicaDocumentRegistry`] owns the id and acts or
//! no-ops — USD, scripting, SysML can install parallel observers that
//! handle *their* ids with no coordination needed.
//!
//! Modelica-specific intents live here too. [`CompileModel`] is the
//! big one: it replaces the old `dispatch_compile_from_buffer` helper
//! and reads source directly from the Document (the buffer is already
//! kept in sync via focus-loss / commit-on-switch).

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_doc::DocumentId;
use lunco_doc_bevy::{
    CloseDocument, DocumentSaved, EditorIntent, RedoDocument, SaveAsDocument, SaveDocument,
    UndoDocument,
};
use lunco_workbench::file_ops::{NewDocument, OpenFile};
use std::collections::HashMap;

use lunco_core::{Command, on_command, register_commands};

use crate::ast_extract::hash_content;
use crate::ui::panels::code_editor::EditorBufferState;
use crate::ui::{CompileState, CompileStates, ModelicaDocumentRegistry, WorkbenchState};
use crate::{ModelicaChannels, ModelicaCommand, ModelicaModel};

// ─────────────────────────────────────────────────────────────────────────────
// Modelica-specific commands
// ─────────────────────────────────────────────────────────────────────────────

/// Request to create a new untitled Modelica model and open its tab.
///
/// Matches VS Code's "New File" flow — no name dialog, no Save-As
/// prompt. The observer picks the next free `Untitled<N>` name,
/// allocates an in-memory [`ModelicaDocument`](crate::document::ModelicaDocument)
/// with a `mem://Untitled<N>` marker path, records it in the Package
/// Browser's in-memory list, and triggers an [`OpenTab`](lunco_workbench::OpenTab)
/// so the user lands on the editable tab immediately.
#[Command(default)]
pub struct CreateNewScratchModel {}

/// Request to duplicate a read-only (library) model into a new
/// editable Untitled document.
///
/// The "play with examples" workflow: user drills into
/// `Modelica.Blocks.Examples.PID_Controller`, looks at the diagram,
/// wants to tweak a parameter. Because the MSL class is read-only,
/// we need a second, editable model. This command creates one —
/// same source, stripped of the `within` clause so the copy doesn't
/// claim to live inside `Modelica.*`, opens a fresh tab, leaves the
/// original MSL tab untouched.
///
/// For classes backed by package-aggregated files (e.g.
/// `Blocks/package.mo`), only the target class's source is
/// extracted — otherwise users would get a 150 KB copy of the
/// whole Blocks package as their "Untitled" starting point.
#[Command(default)]
pub struct DuplicateModelFromReadOnly {
    pub source_doc: DocumentId,
}

/// Request to compile a Modelica document and run the resulting
/// simulation.
///
/// Reads the document's *current* source (not any editor buffer — the
/// buffer is expected to have been flushed by the caller via
/// [`ModelicaDocumentRegistry::checkpoint_source`] before firing), parses
/// parameters / inputs, spawns or updates the [`ModelicaModel`] entity
/// linked to the document, marks the [`CompileState`] as
/// [`CompileState::Compiling`], and sends a
/// [`ModelicaCommand::Compile`] to the worker.
///
/// Unknown / foreign ids are no-ops.
/// API readiness probe. Returns immediately with `{"command_id": N}`
/// — touches no state, side-effect-free, safe to fire from a
/// readiness-poll loop. Use this instead of `FitCanvas` (which
/// touches the canvas) or any other state-mutating command when all
/// you want to know is "is the API up yet?".
#[Command(default)]
pub struct Ping {}

#[on_command(Ping)]
fn on_ping(_cmd: Ping) {
    // Intentional no-op. The dispatcher's normal flow already returns
    // `{"command_id": N}` to the caller before this observer fires;
    // emitting nothing further keeps the response cheap.
}

#[Command(default)]
pub struct CompileModel {
    /// The document to compile.
    pub doc: DocumentId,
    /// Optional explicit target class. When `Some`, bypass both the
    /// drilled-in pin and the picker — compile this exact class.
    /// Used by API callers that need deterministic behaviour without
    /// a GUI (cf. spec 033 User Story 1.5).
    pub class: Option<String>,
}

/// Run the Auto-Arrange layout: assign each component of the active
/// class a deterministic grid position and persist it via a batch of
/// `SetPlacement` ops (undo-able as one group). Matches Dymola's
/// **Edit → Auto Arrange** command. The passive open-time fallback
/// stacks components at origin so nothing jumps around; users invoke
/// this to lay out an imported model cleanly in one click.
///
/// Exposed to the LunCo API: `POST /api/commands` with
/// `{"command": "AutoArrangeDiagram", "params": {"doc": 0}}` where
/// `doc = 0` targets the currently-active tab. Kept as a raw `u64`
/// (not `DocumentId`) so the generic `lunco-doc` crate stays free of
/// the bevy-reflect dependency required to cross the API boundary.
#[Command(default)]
pub struct AutoArrangeDiagram {
    /// Raw `DocumentId::raw()` value, or `0` for "the currently-active
    /// Model tab" (useful from API / tests / scripts that don't track
    /// document ids).
    pub doc: DocumentId,
}

// ─────────────────────────────────────────────────────────────────────────────
// API navigation commands — reflect-registered so scripts / tests /
// remote agents can drive the UI over HTTP without a mouse. Each is a
// fire-and-forget event with a tiny observer; all follow the same
// convention as `AutoArrangeDiagram` (doc=0 means "the active tab").
// ─────────────────────────────────────────────────────────────────────────────

/// Focus (open + bring to front) the tab whose title contains the
/// given substring. Case-sensitive; first match wins.
///
/// Useful from the API because the raw `DocumentId` is server-minted
/// and not discoverable from outside; the tab title is. A future
/// `ListDocuments` query will return the ids directly for exact
/// targeting.
#[Command(default)]
pub struct FocusDocumentByName {
    pub pattern: String,
}

/// Switch the active tab's view mode. `mode` is one of
/// `"text"`, `"diagram"`, `"icon"`, `"docs"` (case-insensitive).
/// Unknown modes are ignored.
#[Command(default)]
pub struct SetViewMode {
    /// Doc id, or `0` for the active tab.
    pub doc: DocumentId,
    /// `"text"` | `"diagram"` | `"icon"` | `"docs"`.
    pub mode: String,
}

/// Set the canvas zoom level for a specific diagram. `1.0` = 100 %.
/// `0.0` = fit-all (same as [`FitCanvas`]).
#[Command(default)]
pub struct SetZoom {
    /// Doc id, or `0` for the active tab.
    pub doc: DocumentId,
    /// Absolute zoom. Clamped to the canvas's configured min/max.
    pub zoom: f32,
}

/// Frame the scene so the whole diagram fits in the viewport.
/// Equivalent to the `F` keyboard shortcut.
#[Command(default)]
pub struct FitCanvas {
    /// Doc id, or `0` for the active tab.
    pub doc: DocumentId,
}

/// Pan + zoom the canvas to centre the named component instance and
/// fill ~50% of the viewport. Use to inspect a single icon at a
/// readable size without manual zoom-pan, e.g. for screenshot-based
/// visual checks during automation.
#[Command(default)]
pub struct FocusComponent {
    /// Doc id, or `0` for the active tab.
    pub doc: DocumentId,
    /// Instance name to focus (e.g. `"addSat"`).
    pub name: String,
    /// Optional padding factor — `0.5` (default when 0) leaves the
    /// component at 50% of the viewport's smaller dim. Larger values
    /// zoom out more.
    pub padding: f32,
}

/// Open (or focus, if already open) an MSL class as a fresh editable
/// copy. `qualified` is the full dot-path,
/// e.g. `"Modelica.Electrical.Analog.Examples.ChuaCircuit"`.
/// Reflect-registered shim over the existing `OpenExampleInWorkspace`
/// event so scripts can open examples without knowing the internal
/// event name.
#[Command(default)]
pub struct OpenExample {
    pub qualified: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Observers
// ─────────────────────────────────────────────────────────────────────────────

/// One entry in the compile-time class picker — captured when the
/// user hit Compile on a doc that's a package of ≥2 models without
/// having drilled into one.
#[derive(Debug, Clone)]
pub struct CompileClassPickerEntry {
    pub doc: DocumentId,
    /// Fully qualified class paths (e.g. `"AnnotatedRocketStage.RocketStage"`).
    pub candidates: Vec<String>,
    /// Index into `candidates` the modal's radio group starts on.
    pub preselected: usize,
    /// What to do once the user confirms a class. Lets the same
    /// picker serve both Compile and Fast Run without duplicating
    /// the modal UI.
    pub purpose: PickerPurpose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerPurpose {
    #[default]
    Compile,
    FastRun,
}

/// Modal picker state for the "which class in this package to
/// compile?" prompt. `None` = no picker open; `Some(entry)` = modal
/// visible. See `render_compile_class_picker` in `ui/mod.rs`.
#[derive(Resource, Default)]
pub struct CompileClassPickerState(pub Option<CompileClassPickerEntry>);

/// Pre-flight dialog state for Fast Run. Mirrors Dymola's
/// "Simulation Setup" modal: confirm bounds before kicking off the
/// batch simulation. Populated by the Fast Run toolbar button;
/// rendered by [`render_fast_run_setup`]; on confirm dispatches
/// `FastRunActiveModel` (which re-reads bounds from the draft this
/// dialog wrote into).
#[derive(Resource, Default)]
pub struct FastRunSetupState(pub Option<FastRunSetupEntry>);

#[derive(Debug, Clone)]
pub struct FastRunSetupEntry {
    pub doc: DocumentId,
    pub model_ref: lunco_experiments::ModelRef,
    pub bounds: lunco_experiments::RunBounds,
    /// Set when overrides are non-empty so the dialog hint nudges
    /// users toward the Experiments panel for full editing.
    pub overrides_count: usize,
}

pub(crate) fn render_fast_run_setup(
    mut egui_ctx: bevy_egui::EguiContexts,
    mut setup: ResMut<FastRunSetupState>,
    mut drafts: ResMut<crate::experiments_runner::ExperimentDrafts>,
    mut commands: Commands,
) {
    let Ok(ctx) = egui_ctx.ctx_mut() else {
        return;
    };
    let Some(entry) = setup.0.as_mut() else {
        return;
    };

    let mut confirmed = false;
    let mut cancelled = false;
    let mut window_open = true;
    egui::Window::new("Simulation Setup — Fast Run")
        .id(egui::Id::new(("fast_run_setup", entry.doc.raw())))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(format!("Class: {}", entry.model_ref.0))
                    .strong(),
            );
            ui.add_space(6.0);
            egui::Grid::new("fastrun_setup_grid")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Start time");
                    ui.add(
                        egui::DragValue::new(&mut entry.bounds.t_start)
                            .speed(0.1)
                            .suffix(" s"),
                    );
                    ui.end_row();

                    ui.label("Stop time");
                    ui.add(
                        egui::DragValue::new(&mut entry.bounds.t_end)
                            .speed(0.1)
                            .suffix(" s"),
                    );
                    ui.end_row();

                    ui.label("dt");
                    let mut adaptive = entry.bounds.dt.is_none();
                    let mut dt_v = entry.bounds.dt.unwrap_or(0.01);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut adaptive, "adaptive").changed() {
                            entry.bounds.dt =
                                if adaptive { None } else { Some(0.01) };
                        }
                        if !adaptive
                            && ui
                                .add(
                                    egui::DragValue::new(&mut dt_v)
                                        .speed(0.001)
                                        .range(1e-6..=10.0),
                                )
                                .changed()
                        {
                            entry.bounds.dt = Some(dt_v);
                        }
                    });
                    ui.end_row();

                    ui.label("Tolerance");
                    let mut tol_on = entry.bounds.tolerance.is_some();
                    let mut tol_v = entry.bounds.tolerance.unwrap_or(1e-6);
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut tol_on, "set").changed() {
                            entry.bounds.tolerance =
                                if tol_on { Some(1e-6) } else { None };
                        }
                        if tol_on
                            && ui
                                .add(
                                    egui::DragValue::new(&mut tol_v)
                                        .speed(1e-7)
                                        .range(1e-12..=1.0),
                                )
                                .changed()
                        {
                            entry.bounds.tolerance = Some(tol_v);
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(6.0);
            if entry.overrides_count > 0 {
                ui.colored_label(
                    egui::Color32::from_rgb(180, 180, 100),
                    format!(
                        "{} parameter override(s) active — edit in 🧪 Experiments",
                        entry.overrides_count
                    ),
                );
            } else {
                ui.weak("Tip: open 🧪 Experiments → ⚙ Overrides + Bounds to override parameters.");
            }
            ui.add_space(8.0);

            // Validation
            let valid = entry.bounds.t_end > entry.bounds.t_start;
            ui.horizontal(|ui| {
                let run = ui.add_enabled(
                    valid,
                    egui::Button::new(
                        egui::RichText::new("⏩ Run").strong(),
                    ),
                );
                if run.clicked() {
                    confirmed = true;
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                if !valid {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "Stop time must be greater than start time",
                    );
                }
            });
        });

    if !window_open {
        cancelled = true;
    }
    if confirmed {
        let entry = setup.0.take().unwrap();
        // Persist edited bounds into the draft so FastRunActiveModel
        // picks them up. Overrides untouched.
        drafts
            .entry(entry.model_ref.clone())
            .bounds_override = Some(entry.bounds);
        commands.trigger(FastRunActiveModel { doc: entry.doc });
    } else if cancelled {
        setup.0 = None;
    }
}

/// Render the compile-class picker modal when
/// [`CompileClassPickerState`] is populated. Confirming re-dispatches
/// `CompileModel` with the chosen class stamped into
/// [`DrilledInClassNames`] so downstream observers see the user's
/// pick exactly as they would've after a manual drill-in. Cancel
/// just clears the state.
pub(crate) fn render_compile_class_picker(
    mut egui_ctx: bevy_egui::EguiContexts,
    mut picker: ResMut<CompileClassPickerState>,
    mut tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut commands: Commands,
) {
    let Ok(ctx) = egui_ctx.ctx_mut() else {
        return;
    };
    let Some(entry) = picker.0.as_mut() else {
        return;
    };

    let mut confirmed: Option<String> = None;
    let mut cancelled = false;
    let mut window_open = true;
    let title = match entry.purpose {
        PickerPurpose::Compile => "Which class should Compile run?",
        PickerPurpose::FastRun => "Which class should Fast Run simulate?",
    };
    egui::Window::new(title)
        .id(egui::Id::new(("compile_class_picker", entry.doc.raw())))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut window_open)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "This file is a package with more than one model. Pick \
                     the class you want to compile:",
                )
                .size(12.0),
            );
            ui.add_space(8.0);
            let mut selected = entry.preselected.min(entry.candidates.len().saturating_sub(1));
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    for (i, name) in entry.candidates.iter().enumerate() {
                        ui.radio_value(&mut selected, i, name);
                    }
                });
            entry.preselected = selected;
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let ok_label = match entry.purpose {
                    PickerPurpose::Compile => "Compile",
                    PickerPurpose::FastRun => "Fast Run",
                };
                let ok = ui.add(egui::Button::new(
                    egui::RichText::new(ok_label).strong(),
                ));
                if ok.clicked() {
                    confirmed = entry.candidates.get(selected).cloned();
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
                ui.add_space(10.0);
                ui.colored_label(
                    egui::Color32::from_rgb(160, 160, 180),
                    "Tip: drill into a class (Canvas / Package Browser) \
                     to skip this dialog next time.",
                );
            });
        });
    if !window_open {
        cancelled = true;
    }
    if let Some(qualified) = confirmed {
        let doc = entry.doc;
        let purpose = entry.purpose;
        // B.3 phase 3: write the picked class onto every tab
        // viewing this doc so subsequent reads via
        // `drilled_class_for_doc` see the user's choice. Replaces
        // the legacy `DrilledInClassNames` cache write.
        for (_, state) in tabs.iter_mut_for_doc(doc) {
            state.drilled_class = Some(qualified.clone());
        }
        picker.0 = None;
        match purpose {
            PickerPurpose::Compile => {
                commands.trigger(CompileModel { doc, class: None });
            }
            PickerPurpose::FastRun => {
                // Re-dispatch — second-time-around the drilled-class
                // pin is set so resolution skips the picker.
                commands.trigger(FastRunActiveModel { doc });
            }
        }
    } else if cancelled {
        picker.0 = None;
    }
}

/// Plugin that installs all Modelica command observers.
///
/// `ModelicaUiPlugin` adds this automatically. Keeping the registration
/// in its own plugin makes it easy for headless tests (or another shell
/// that doesn't want the rest of the UI plugin) to opt in to the
/// command path alone.
pub struct ModelicaCommandsPlugin;

impl Plugin for ModelicaCommandsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CloseDialogState>()
            .init_resource::<PendingCloseAfterSave>()
            .init_resource::<CompileClassPickerState>()
            .init_resource::<FastRunSetupState>()
            .add_observer(on_undo_document)
            .add_observer(on_redo_document)
            .add_observer(on_save_document)
            .add_observer(on_save_as_document)
            .add_observer(finish_close_after_save)
            .add_observer(on_close_document)
            .add_observer(on_document_closed_cleanup)
            // Register the `modelica://` scheme with the workbench's
            // cross-domain URI registry so clickable links in
            // Documentation HTML (and any future contexts) route
            // through the same plumbing as USD / SysML will when
            // their domain crates land.
            .add_observer(crate::ui::uri_handler::on_modelica_uri_clicked)
            // Auto-Arrange: reflect-registered so the LunCo API can
            // fire it via `ExecuteCommand { command: "AutoArrangeDiagram" }`.
            .register_type::<AutoArrangeDiagram>()
            .add_observer(crate::ui::panels::canvas_diagram::on_auto_arrange_diagram)
            // Compile: reflect-registered so the HTTP API can drive
            // headless / scripted / CI compilation. Without this the
            // dispatcher rejects with "Command 'CompileModel' not
            // found or not API-accessible" and only UI clicks work.
            // Ping: side-effect-free readiness probe, the proper
            // alternative to using FitCanvas (or any other state-
            // mutating command) for "is the API up?" polling.
            // Registered separately via the macro-generated helper
            // (see `__register_on_ping(app)` call below the chain).
            // Navigation commands — same reflect-registered pattern so
            // the HTTP API can drive the UI (focus a tab, switch view
            // mode, zoom / fit, drill into an MSL example).
            .add_observer(resolve_editor_intent)
            .add_observer(resolve_new_document_intent)
            // Install our scheme handler into the workbench's
            // `UriRegistry` once at startup. The registry resource is
            // inserted by WorkbenchPlugin; this system runs after it,
            // pushes the handler, and exits.
            .add_systems(
                bevy::prelude::Startup,
                (register_modelica_uri_handler, prewarm_msl_library),
            )
            .add_systems(
                bevy::prelude::Update,
                (
                    drain_pending_tab_closes,
                    update_status_bar,
                    publish_unsaved_modelica_docs,
                ),
            )
            .add_systems(
                bevy_egui::EguiPrimaryContextPass,
                (
                    render_close_dialogs,
                    render_compile_class_picker,
                    render_fast_run_setup,
                ),
            );

        // All observers marked with `#[on_command(X)]` are registered
        // in one shot via the `register_commands!()`-generated helper
        // (see the macro invocation just below this `impl`). Adding a
        // new typed command is now: write the struct + observer with
        // their attributes, then add one identifier to the list.
        register_all_commands(app);
    }
}

// Single source-of-truth for which typed commands this plugin owns.
// `register_commands!()` expands to a `pub fn register_all_commands(app)`
// that calls the per-observer `__register_on_X(app)` helpers
// (themselves generated by `#[on_command(X)]`). Keep alphabetical so
// diffs stay tidy.
register_commands!(
    on_add_canvas_plot,
    on_add_signal_to_plot,
    on_compile_active_model,
    on_compile_model,
    on_create_new_scratch_model,
    on_duplicate_active_doc,
    on_duplicate_model_from_read_only,
    on_exit,
    on_fast_run_active_model,
    on_fit_canvas,
    on_focus_component,
    on_focus_document_by_name,
    on_format_document,
    on_get_file,
    on_inspect_active_doc,
    on_move_component,
    on_new_modelica_document,
    on_new_plot_panel,
    on_open,
    on_open_class,
    on_open_example,
    on_open_file,
    on_pan_canvas,
    on_pause_active_model,
    on_ping,
    on_redo,
    on_reset_active_model,
    on_resume_active_model,
    on_save_active_document,
    on_save_active_document_as,
    on_set_model_input,
    on_set_view_mode,
    on_set_zoom,
    on_undo,
);

// ─────────────────────────────────────────────────────────────────────────────
// Unsaved-changes close prompt
// ─────────────────────────────────────────────────────────────────────────────

/// Per-doc confirmation state for "close tab with unsaved changes".
///
/// The [`CloseTab`](lunco_workbench::CloseTab) event on a dirty doc is
/// gated by this queue: the workbench's on-close hook pushes the tab
/// id into `PendingTabCloses`, `drain_pending_tab_closes` inspects the
/// dirty flag, and dirty tabs land here to await a user decision. The
/// `render_close_dialogs` system draws a modal per entry.
#[derive(Resource, Default)]
pub struct CloseDialogState {
    /// Docs with an open close-confirmation modal.
    pub pending: Vec<DocumentId>,
}

/// Drain `PendingTabCloses` from `lunco_workbench`. Clean docs close
/// immediately; dirty docs get queued for the user-confirmation modal.
///
/// Documents for which the user chose **Save** in the close
/// confirmation dialog. Once each doc fires its `DocumentSaved`, the
/// close completes; if the save is cancelled (Save-As picker dismissed
/// for an Untitled) the doc stays in place and the tab keeps living,
/// matching VS Code's behaviour.
#[derive(Resource, Default)]
pub struct PendingCloseAfterSave {
    docs: std::collections::HashSet<DocumentId>,
}

impl PendingCloseAfterSave {
    fn queue(&mut self, doc: DocumentId) {
        self.docs.insert(doc);
    }
    fn take(&mut self, doc: DocumentId) -> bool {
        self.docs.remove(&doc)
    }
}

/// Observer: after a `DocumentSaved`, finish any close that was
/// waiting on this save. Fires `CloseTab` + `CloseDocument` in order.
fn finish_close_after_save(
    trigger: On<lunco_doc_bevy::DocumentSaved>,
    pending: Option<ResMut<PendingCloseAfterSave>>,
    mut commands: Commands,
) {
    let Some(mut pending) = pending else { return };
    let doc = trigger.event().doc;
    if pending.take(doc) {
        // Multiple tabs may view the same doc (e.g. user opened
        // sibling drill-ins or a future Text+Canvas split). Close
        // every tab on this doc, then drop the doc.
        commands.queue(move |world: &mut World| {
            let tab_ids: Vec<u64> = world
                .resource::<crate::ui::panels::model_view::ModelTabs>()
                .iter()
                .filter_map(|(id, s)| (s.doc == doc).then_some(id))
                .collect();
            for tab_id in tab_ids {
                world.commands().trigger(lunco_workbench::CloseTab {
                    kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                    instance: tab_id,
                });
            }
            world.commands().trigger(CloseDocument { doc });
        });
    }
}

/// Runs on `Update`, so it picks up both the tab × button (queued by
/// the workbench's `on_close`) and Ctrl+W (pushed by the
/// EditorIntent::Close resolver below).
fn drain_pending_tab_closes(
    mut pending: ResMut<lunco_workbench::PendingTabCloses>,
    registry: Res<ModelicaDocumentRegistry>,
    mut model_tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut dialogs: ResMut<CloseDialogState>,
    mut commands: Commands,
) {
    for tab in pending.drain() {
        let lunco_workbench::TabId::Instance { kind, instance } = tab else {
            continue; // Singleton — not our concern.
        };
        // VizPanel (multi-instance plot) tabs close immediately —
        // they have no dirty state to confirm. Without this branch the
        // × button on a "Plot #N" tab queued the close and the tab
        // never went away.
        if kind == lunco_viz::VIZ_PANEL_KIND {
            commands.trigger(lunco_workbench::CloseTab { kind, instance });
            commands.queue(move |world: &mut World| {
                if let Some(mut reg) =
                    world.get_resource_mut::<lunco_viz::VisualizationRegistry>()
                {
                    reg.remove(lunco_viz::viz::VizId(instance));
                }
            });
            continue;
        }
        if kind != crate::ui::panels::model_view::MODEL_VIEW_KIND {
            continue; // Another domain's tab.
        }
        // `instance` is now an opaque TabId — look up the doc that
        // tab views.
        let Some(doc) = model_tabs.get(instance).map(|s| s.doc) else {
            // Tab vanished between request and drain. Forward the
            // close so the dock layer drops it; nothing to do here.
            commands.trigger(lunco_workbench::CloseTab { kind, instance });
            continue;
        };
        let (is_dirty, is_read_only) = registry
            .host(doc)
            .map(|h| {
                let d = h.document();
                (d.is_dirty(), d.is_read_only())
            })
            .unwrap_or((false, false));
        // Read-only docs (MSL library classes the user drilled into)
        // have nothing to save — the dialog's Save button is disabled
        // for them anyway. Skip the prompt entirely and close.
        if is_dirty && !is_read_only {
            if !dialogs.pending.contains(&doc) {
                dialogs.pending.push(doc);
            }
        } else {
            // Drop just this tab; only close the document when its
            // last tab is going away. Multiple tabs viewing the
            // same doc (split-view, sibling drill-ins) must outlive
            // each individual close.
            commands.trigger(lunco_workbench::CloseTab { kind, instance });
            model_tabs.close_tab(instance);
            // Per-tab canvas state (viewport, selection, scene)
            // dies with the tab. Doc-wide cleanup happens later in
            // `on_document_closed_cleanup` if this was the last tab.
            commands.queue(move |world: &mut World| {
                if let Some(mut state) = world
                    .get_resource_mut::<crate::ui::panels::canvas_diagram::CanvasDiagramState>()
                {
                    state.drop_tab(instance);
                }
            });
            if model_tabs.count_for_doc(doc) == 0 {
                commands.trigger(CloseDocument { doc });
            }
        }
    }
}

/// Render one modal per entry in [`CloseDialogState`]. Three choices:
/// **Save** (disabled for Untitled until Save-As lands), **Don't save**,
/// **Cancel**. The Save path fires `SaveDocument` + full close; Don't
/// save fires the close directly; Cancel dismisses the dialog.
fn render_close_dialogs(
    mut egui_ctx: bevy_egui::EguiContexts,
    registry: Res<ModelicaDocumentRegistry>,
    mut dialogs: ResMut<CloseDialogState>,
    // `Option<ResMut>` rather than `ResMut` — the system is registered
    // in one of the `EguiPrimaryContextPass` passes which, in Bevy
    // 0.18, can be polled before plugin-level `init_resource`s have
    // taken effect on a world that was externally-constructed (e.g.
    // the minimal-app CI path). Missing resource is a no-op; normal
    // runs always populate it from `ModelicaCommandsPlugin::build`.
    mut pending_save_close: Option<ResMut<PendingCloseAfterSave>>,
    mut commands: Commands,
) {
    let Ok(ctx) = egui_ctx.ctx_mut() else {
        return;
    };
    // Drain-and-reinsert pattern so we can mutate individual entries
    // without fighting the Vec during iteration.
    let pending = std::mem::take(&mut dialogs.pending);
    let mut survivors = Vec::with_capacity(pending.len());
    for doc in pending {
        let Some(host) = registry.host(doc) else {
            // Doc vanished (another system closed it). Drop the dialog.
            continue;
        };
        let document = host.document();
        let display_name = document.origin().display_name();
        let is_untitled = document.origin().is_untitled();
        let is_read_only = document.is_read_only();
        // Read-only library classes can't be saved at all; the user's
        // only honest options are Don't Save or Cancel. Untitled docs
        // route their Save through Save-As → the picker.
        let can_save = !is_read_only;

        enum DialogAction {
            None,
            Save,
            DontSave,
            Cancel,
        }
        let mut action = DialogAction::None;

        let window_id = egui::Id::new(("unsaved_close_prompt", doc.raw()));
        let mut open = true;
        egui::Window::new(format!("Save changes to '{}'?", display_name))
            .id(window_id)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Your changes will be lost if you don't save them.",
                    )
                    .size(12.0),
                );
                if is_untitled {
                    ui.add_space(4.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(180, 180, 200),
                        "This model has never been saved — picking Save \
                         will open a Save-As dialog to bind it to a file.",
                    );
                }
                if is_read_only {
                    ui.add_space(4.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 150, 50),
                        "This is a read-only library class; Save is \
                         unavailable. Use Duplicate to Workspace if you \
                         want to keep your edits.",
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let save_btn = ui.add_enabled(
                        can_save,
                        egui::Button::new(egui::RichText::new("Save").strong()),
                    );
                    if save_btn.clicked() {
                        action = DialogAction::Save;
                    }
                    if ui.button("Don't save").clicked() {
                        action = DialogAction::DontSave;
                    }
                    if ui.button("Cancel").clicked() {
                        action = DialogAction::Cancel;
                    }
                });
            });
        // Close via the title-bar X also dismisses — treat as Cancel.
        if !open {
            action = DialogAction::Cancel;
        }

        match action {
            DialogAction::None => {
                survivors.push(doc);
            }
            DialogAction::Save => {
                // Queue the close to run *after* the save completes —
                // for Untitled docs the save opens a picker that the
                // user may cancel, in which case the close must NOT
                // proceed. `finish_close_after_save` observer fires
                // CloseTab+CloseDocument when DocumentSaved lands.
                if let Some(q) = pending_save_close.as_mut() {
                    q.queue(doc);
                }
                commands.trigger(SaveDocument { doc });
            }
            DialogAction::DontSave => {
                // Close every tab pointing at this doc, then drop
                // the doc. Multi-tab on a single doc (drill-in
                // siblings, split-view) must all go together — the
                // doc is what's being abandoned.
                commands.queue(move |world: &mut World| {
                    let tab_ids: Vec<u64> = world
                        .resource::<crate::ui::panels::model_view::ModelTabs>()
                        .iter()
                        .filter_map(|(id, s)| (s.doc == doc).then_some(id))
                        .collect();
                    for tab_id in tab_ids {
                        world.commands().trigger(lunco_workbench::CloseTab {
                            kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                            instance: tab_id,
                        });
                    }
                    world.commands().trigger(CloseDocument { doc });
                });
            }
            DialogAction::Cancel => { /* drop from pending */ }
        }
    }
    dialogs.pending = survivors;
}

/// Observer fired after a document is removed from the registry.
/// Cleans up the domain-side state that trailed the document:
/// `ModelTabs` entry, `PackageTreeCache.in_memory_models` entry,
/// `CompileStates` entry.
fn on_document_closed_cleanup(
    trigger: On<lunco_doc_bevy::DocumentClosed>,
    mut model_tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut cache: ResMut<crate::ui::panels::package_browser::PackageTreeCache>,
    mut compile_states: ResMut<CompileStates>,
    mut workbench: ResMut<WorkbenchState>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
) {
    let doc = trigger.event().doc;
    model_tabs.close(doc);
    cache.in_memory_models.retain(|e| e.doc != doc);
    compile_states.remove(doc);
    // If the closed doc was active, clear the slot so the welcome
    // panel / another tab's sync can take over. Drive the check off
    // `workspace.active_document` (the source of truth) and reset
    // both the Workspace pointer and the UI cache in lockstep.
    if workspace.active_document == Some(doc) {
        workspace.active_document = None;
        // B.3 phase 6: `open_model` cache retired.
        workbench.editor_buffer.clear();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Intent resolver — EditorIntent → concrete command for Modelica docs
// ─────────────────────────────────────────────────────────────────────────────

/// Translate an abstract [`EditorIntent`] into the concrete Modelica
/// command(s) it maps to, targeting the currently-active document.
///
/// **Ownership-aware**: only resolves when the active document is
/// owned by [`ModelicaDocumentRegistry`]. If another domain (USD,
/// scripting, SysML) owns the active doc, its own resolver handles
/// the intent and this observer no-ops — both resolvers fire on
/// every intent and each picks the ones that belong to it.
///
/// This is the "intent → command" layer. Keybindings map keys to
/// intents in `lunco-doc-bevy`; resolvers like this one map intents
/// to concrete commands per domain. Users reconfiguring hotkeys
/// never touch this function; they edit their `Keybindings`.
fn resolve_editor_intent(
    trigger: On<EditorIntent>,
    workspace: Res<lunco_workbench::WorkspaceResource>,
    registry: Res<ModelicaDocumentRegistry>,
    mut pending_closes: ResMut<lunco_workbench::PendingTabCloses>,
    mut commands: Commands,
) {
    let Some(doc) = workspace.active_document else {
        return;
    };
    // Ownership check — is this doc in the Modelica registry?
    if registry.host(doc).is_none() {
        return;
    }

    match *trigger.event() {
        EditorIntent::Undo => commands.trigger(UndoDocument { doc }),
        EditorIntent::Redo => commands.trigger(RedoDocument { doc }),
        EditorIntent::Save => commands.trigger(SaveDocument { doc }),
        EditorIntent::SaveAs => commands.trigger(SaveAsDocument { doc, path: String::new() }),
        EditorIntent::Close => {
            // Ctrl+W goes through the same dirty-check + modal-prompt
            // pipeline as the tab × button. Push the tab id into the
            // workbench's close-request queue; `drain_pending_tab_closes`
            // decides whether to close immediately or prompt. Resolve
            // a TabId for the active doc — Ctrl+W has no built-in
            // notion of "which" tab when split-view is in use, so
            // close any tab viewing the active doc.
            commands.queue(move |world: &mut World| {
                let Some(tab_id) = world
                    .resource::<crate::ui::panels::model_view::ModelTabs>()
                    .any_for_doc(doc)
                else {
                    return;
                };
                if let Some(mut q) = world
                    .get_resource_mut::<lunco_workbench::PendingTabCloses>()
                {
                    q.push(lunco_workbench::TabId::Instance {
                        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                        instance: tab_id,
                    });
                }
            });
            // `pending_closes` ResMut still required by signature; we
            // do the queueing ourselves so we can read ModelTabs.
            let _ = &mut pending_closes;
        }
        // Per AGENTS.md §4.1 rule 3: UI / keybinding gestures fire the
        // public Reflect command (`CompileActiveModel`), not the
        // internal observer event (`CompileModel`). Empty `class`
        // inherits the picker / drilled-in / detected-name behaviour,
        // matching the pre-migration semantics for keyboard Compile.
        EditorIntent::Compile => {
            commands.trigger(CompileActiveModel {
                doc,
                class: String::new(),
            });
        }
        // `NewDocument` doesn't need an active doc — it's handled by
        // `NewDocumentNoDoc` resolver below (the resolver that runs
        // even when there's no active doc).
        EditorIntent::NewDocument => {}
    }
}

/// Second EditorIntent resolver that fires regardless of whether an
/// active document is owned by Modelica — handles the intents that
/// have no existing-doc target, currently just `NewDocument`.
///
/// Kept separate from [`resolve_editor_intent`] so the active-doc
/// ownership check there can stay a hard precondition for all other
/// intent variants.
fn resolve_new_document_intent(trigger: On<EditorIntent>, mut commands: Commands) {
    if matches!(*trigger.event(), EditorIntent::NewDocument) {
        // Empty `kind` is the workbench's "use the default" sentinel —
        // it looks up `DocumentKindRegistry`, picks the first kind
        // with `can_create_new=true`, and re-fires. That lets Ctrl+N
        // pick a sensible default in any app composition (Modelica
        // alone, Modelica+USD+Julia, …) without this resolver knowing
        // who's loaded.
        commands.trigger(NewDocument {
            kind: String::new(),
        });
    }
}

/// Modelica's branch of the kind-typed [`NewDocument`] command.
///
/// Gates on `kind == "modelica"` and forwards to
/// [`CreateNewScratchModel`] — which already owns the
/// "find next free Untitled<N>" + scratch-buffer + tab-spawn flow.
/// Other domains' observers gate on their own kinds and ignore this
/// one's payload.
#[on_command(NewDocument)]
fn on_new_modelica_document(trigger: On<NewDocument>, mut commands: Commands) {
    if trigger.event().kind != "modelica" {
        return;
    }
    commands.trigger(CreateNewScratchModel {});
}

fn on_undo_document(
    trigger: On<UndoDocument>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut editor: ResMut<EditorBufferState>,
    mut workbench: ResMut<WorkbenchState>,
) {
    let doc = trigger.event().doc;
    apply_undo_or_redo(
        doc,
        /*is_undo=*/ true,
        &mut registry,
        &mut editor,
        &mut workbench,
    );
}

fn on_redo_document(
    trigger: On<RedoDocument>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut editor: ResMut<EditorBufferState>,
    mut workbench: ResMut<WorkbenchState>,
) {
    apply_undo_or_redo(
        trigger.event().doc,
        /*is_undo=*/ false,
        &mut registry,
        &mut editor,
        &mut workbench,
    );
}

/// Shared body for Undo / Redo — runs the op on the `DocumentHost`,
/// then mirrors the reverted source into the editor buffer so the
/// text view shows it on the next frame.
///
/// No-op if the registry doesn't own `doc`, if there's nothing to
/// undo/redo, or if the document is read-only.
fn apply_undo_or_redo(
    doc: DocumentId,
    is_undo: bool,
    registry: &mut ModelicaDocumentRegistry,
    editor: &mut EditorBufferState,
    workbench: &mut WorkbenchState,
) {
    // Ownership check only — `Document::is_read_only()` here means
    // "can't save without Save-As", which is true for every Untitled
    // doc (Duplicate-to-Workspace copies, freshly-typed scratch
    // models). Those are fully editable; the predicate's name is
    // misleading. The canvas's apply_ops gates on
    // `Document::is_read_only()` (true only for bundled / library
    // tabs); we mirror that here so undo/redo works on Untitled
    // docs.
    if registry.host(doc).is_none() {
        return;
    }
    // B.3 phase 6: read read-only from the document directly.
    let workbench_read_only = registry
        .host(doc)
        .map(|h| {
            use lunco_doc::Document as _;
            h.document().is_read_only()
        })
        .unwrap_or(false);
    let _ = workbench;
    if workbench_read_only {
        return;
    }

    let new_source = {
        let result = registry.host_mut(doc).and_then(|host| {
            let changed = if is_undo {
                host.undo().ok().unwrap_or(false)
            } else {
                host.redo().ok().unwrap_or(false)
            };
            changed.then(|| host.document().source().to_string())
        });
        // Undo/redo goes directly through `host_mut` — record it so the
        // Bevy observer drain sees the change.
        if result.is_some() {
            registry.mark_changed(doc);
        }
        result
    };

    let Some(source) = new_source else { return };
    sync_editor_buffer_to_source(&source, editor, workbench);
}

/// Write the given source into [`EditorBufferState`] (including line
/// starts, detected name, hash) and [`WorkbenchState::editor_buffer`]
/// so both the text view and any mirror consumers see the new content
/// on the next frame.
fn sync_editor_buffer_to_source(
    source: &str,
    editor: &mut EditorBufferState,
    workbench: &mut WorkbenchState,
) {
    let mut new_starts = vec![0usize];
    for (i, b) in source.as_bytes().iter().enumerate() {
        if *b == b'\n' {
            new_starts.push(i + 1);
        }
    }
    editor.text = source.to_string();
    editor.line_starts = new_starts.into();
    // NOTE: do NOT call `extract_model_name(source)` here. It runs a
    // full rumoca parse synchronously on the main thread (see
    // `ast_extract::extract_model_name` doc) and stalls the UI on
    // every undo / redo / restore. The worker reparses off-thread
    // via the `DocumentChanged` pipeline that callers fire after
    // mutating the document, and that path refreshes the per-doc
    // Index that UI consumers (inspector, canvas overlays) read.
    editor.source_hash = hash_content(source);
    workbench.editor_buffer = source.to_string();
}

fn on_save_document(
    trigger: On<SaveDocument>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut console: ResMut<crate::ui::panels::console::ConsoleLog>,
    mut commands: Commands,
) {
    let doc = trigger.event().doc;

    // Validate + snapshot what we need to write.
    let to_save = {
        let Some(host) = registry.host(doc) else {
            return; // Foreign / unknown id.
        };
        let document = host.document();
        // Untitled → route through Save-As so the user picks a path.
        // Matches VS Code's behaviour (Ctrl+S on an Untitled buffer
        // opens the Save-As dialog).
        if document.origin().is_untitled() {
            commands.trigger(SaveAsDocument { doc, path: String::new() });
            return;
        }
        let Some(path) = document.canonical_path() else {
            console.warn(format!(
                "Save skipped — doc {doc} has no canonical path"
            ));
            return;
        };
        if document.is_read_only() {
            let name = document.origin().display_name();
            let msg = format!("Save blocked — '{name}' is read-only (library / bundled example).");
            warn!("[Save] {msg}");
            console.warn(msg);
            return;
        }
        (path.to_path_buf(), document.source().to_string())
    };

    let (path, source) = to_save;
    // Write through `lunco-storage` so the backend seam is exercised
    // (native today, OPFS / IndexedDB / HTTP tomorrow — same trait).
    let storage = lunco_storage::FileStorage::new();
    let handle = lunco_storage::StorageHandle::File(path.clone());
    if let Err(e) = <lunco_storage::FileStorage as lunco_storage::Storage>::write(
        &storage,
        &handle,
        source.as_bytes(),
    ) {
        let msg = format!("Save failed: {}: {e}", path.display());
        error!("[Save] {msg}");
        console.error(msg);
        return;
    }
    let msg = format!("Saved {} bytes to {}", source.len(), path.display());
    info!("[Save] {msg}");
    console.info(msg);

    registry.mark_document_saved(doc);
    commands.trigger(DocumentSaved::local(doc));
}

/// Observer for [`SaveAsDocument`].
///
/// Two-phase flow keyed on the `path` field:
///
/// 1. **`path` empty** — assemble a [`SaveHint`](lunco_storage::SaveHint)
///    (suggested filename from doc display name, start dir from the
///    active Twin's folder, `.mo` filter) and fire
///    [`PickHandle`](lunco_workbench::picker::PickHandle) with
///    [`PickFollowUp::SaveAs(doc)`](lunco_workbench::picker::PickFollowUp).
///    The workbench's `on_pick_resolved` re-fires `SaveAsDocument`
///    with the chosen path. Cancellation is silent — no second fire.
///
/// 2. **`path` non-empty** — write the document's source via
///    [`Storage::write`](lunco_storage::Storage), rebind its origin to
///    the new writable [`File`](lunco_doc::DocumentOrigin) variant,
///    mark saved, fire [`DocumentSaved`].
///
/// The two-phase split keeps the observer synchronous (no blocking
/// rfd on the UI thread) and gives every caller — Ctrl+Shift+S, menu,
/// HTTP automation, recents — the same shape: trigger
/// `SaveAsDocument { doc, path: "" }` to force the dialog, or supply
/// `path` to skip it.
fn on_save_as_document(
    trigger: On<SaveAsDocument>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    workspace: Res<lunco_workbench::WorkspaceResource>,
    mut console: ResMut<crate::ui::panels::console::ConsoleLog>,
    mut commands: Commands,
) {
    let doc = trigger.event().doc;
    let target_path = trigger.event().path.clone();

    // Phase 1 — empty path: fire the picker and bail.
    if target_path.is_empty() {
        let Some(host) = registry.host(doc) else { return };
        let document = host.document();
        let suggested_name = {
            let raw = document.origin().display_name();
            // Attach `.mo` if the user hasn't already chosen a full
            // filename (Untitled<N> is the common case).
            if raw.ends_with(".mo") {
                raw.to_string()
            } else {
                format!("{raw}.mo")
            }
        };
        // Start in the active Twin's folder so Save-As of a scratch
        // doc lands inside the project the user is working on by
        // default. Falls through to the picker's default when no
        // active Twin is set.
        let start_dir = workspace
            .active_twin
            .and_then(|id| workspace.twin(id))
            .map(|t| lunco_storage::StorageHandle::File(t.root.clone()));
        commands.trigger(lunco_workbench::picker::PickHandle {
            mode: lunco_workbench::picker::PickMode::SaveFile(lunco_storage::SaveHint {
                suggested_name: Some(suggested_name),
                start_dir,
                filters: vec![lunco_storage::OpenFilter::new("Modelica models", &["mo"])],
            }),
            on_resolved: lunco_workbench::picker::PickFollowUp::SaveAs(doc),
        });
        return;
    }

    // Phase 2 — non-empty path: write directly.
    let path = std::path::PathBuf::from(&target_path);
    let source = {
        let Some(host) = registry.host(doc) else { return };
        host.document().source().to_string()
    };

    let storage = lunco_storage::FileStorage::new();
    let handle = lunco_storage::StorageHandle::File(path.clone());
    if let Err(e) = <lunco_storage::FileStorage as lunco_storage::Storage>::write(
        &storage,
        &handle,
        source.as_bytes(),
    ) {
        let msg = format!("Save-As failed: {}: {e}", path.display());
        error!("[SaveAs] {msg}");
        console.error(msg);
        return;
    }

    // Rebind the document's origin to the new writable path and mark
    // it saved. `set_origin` does not touch source or generation.
    if let Some(host) = registry.host_mut(doc) {
        host.document_mut().set_origin(lunco_doc::DocumentOrigin::File {
            path: path.clone(),
            writable: true,
        });
    }
    registry.mark_document_saved(doc);
    let msg = format!("Saved {} bytes to {}", source.len(), path.display());
    info!("[SaveAs] {msg}");
    console.info(msg);

    commands.trigger(DocumentSaved::local(doc));
}

fn on_close_document(
    trigger: On<CloseDocument>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
) {
    let doc = trigger.event().doc;
    if registry.host(doc).is_none() {
        return; // Foreign or already-closed.
    }
    registry.remove_document(doc);
}

#[on_command(CompileModel)]
fn on_compile_model(
    trigger: On<CompileModel>,
    mut commands: Commands,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut workbench: ResMut<WorkbenchState>,
    mut compile_states: ResMut<CompileStates>,
    mut console: ResMut<crate::ui::panels::console::ConsoleLog>,
    mut diagnostics: Option<ResMut<crate::ui::panels::diagnostics::DiagnosticsLog>>,
    mut picker: ResMut<CompileClassPickerState>,
    mut sim_streams: ResMut<crate::SimStreamRegistry>,
    channels: Option<Res<ModelicaChannels>>,
    mut q_models: Query<&mut ModelicaModel>,
    model_tabs: Res<crate::ui::panels::model_view::ModelTabs>,
) {
    let doc = trigger.event().doc;
    let explicit_class = trigger.event().class.clone();

    // Ownership check. Read-only docs are fair game to compile —
    // the Save button is what's gated on writability, not compile.
    // Users *simulate* examples; they just can't overwrite them.
    //
    // Use the document's already-parsed AST for the metadata
    // extraction. Calling the `_source` variants here re-parses
    // via rumoca on the main thread — a 152 KB MSL package file
    // costs ~30 s per call in debug builds, and there are four
    // calls, so clicking Compile on an MSL example would lock the
    // UI for minutes. Pulling from the cached AST is constant-time.
    // Note: previously this site called `refresh_ast_now()` to force
    // a fresh parse before extracting metadata. That ran a 2.5 s
    // rumoca parse synchronously on the main thread (verified in
    // telemetry: `[Doc] refresh_ast_now: 20052 bytes parsed in
    // 2522.0ms`) and froze the UI — sim-time stalled, egui animations
    // stuttered, FixedUpdate skipped 60+ ticks. The off-thread
    // debounced refresh (see `ui::ast_refresh`) keeps the AST at
    // most 250 ms behind source, which the metadata extractors
    // below (params / inputs / bounds / class names) tolerate fine.
    // The worker re-parses the *source* verbatim for the actual
    // compile (see `ModelicaCommand::Compile`), so any AST staleness
    // here only affects telemetry-panel labels for one debounce
    // cycle, not the compiled model itself.
    let (source, ast_for_extract, candidate_classes, detected_first_class, params, inputs_with_defaults, runtime_inputs) =
        match registry.host(doc) {
            Some(h) => {
                let doc_ref = h.document();
                let ast = doc_ref.strict_ast();
                // Class candidates + first-non-package detection via
                // the per-doc Index (sees optimistic patches; no extra
                // AST walk per call).
                let index = doc_ref.index();
                let candidates: Vec<String> = index
                    .classes
                    .values()
                    .filter(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                    .map(|c| c.name.clone())
                    .collect();
                let first_non_package = index
                    .classes
                    .values()
                    .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                    .map(|c| c.name.clone());
                // Compile-time seed values for `ModelicaModel`
                // (parameters / input defaults / runtime input names)
                // — read straight from the index. Replaces three
                // `ast_extract::extract_*_from_ast` calls that walked
                // the same data.
                let mut params: HashMap<String, f64> = HashMap::new();
                let mut inputs_with_defaults: HashMap<String, f64> = HashMap::new();
                let mut runtime_inputs: Vec<String> = Vec::new();
                for entry in &index.components {
                    let numeric = entry
                        .binding
                        .as_ref()
                        .and_then(|s| s.parse::<f64>().ok());
                    match (entry.variability, entry.causality) {
                        (crate::index::Variability::Parameter, _)
                        | (crate::index::Variability::Constant, _) => {
                            if let Some(v) = numeric {
                                params.insert(entry.name.clone(), v);
                            }
                        }
                        (_, crate::index::Causality::Input) => {
                            if let Some(v) = numeric {
                                inputs_with_defaults.insert(entry.name.clone(), v);
                            } else {
                                runtime_inputs.push(entry.name.clone());
                            }
                        }
                        _ => {}
                    }
                }
                (
                    doc_ref.source().to_string(),
                    ast,
                    candidates,
                    first_non_package,
                    params,
                    inputs_with_defaults,
                    runtime_inputs,
                )
            }
            None => return,
        };
    let Some(ast) = ast_for_extract else {
        // Parse failure on this doc (rare — rumoca is
        // error-recovering). Fall back to the source-based
        // extractors, which at least try once; if they also fail,
        // the error message below fires.
        let msg = "Could not parse Modelica source for compile.".to_string();
        // B.3 phase 4: per-doc error.
        compile_states.set_error(doc, msg.clone());
        console.error(format!("Compile failed: {msg}"));
        return;
    };
    // Prefer the drilled-in class on this doc — the user is looking
    // at a leaf model (e.g. `AnnotatedRocketStageCopy.RocketStage`)
    // and pressing Compile must compile *that*, not the enclosing
    // package. Without this the compile picks the first non-package
    // class (often the package wrapper) and the simulator returns
    // `EmptySystem`.
    // B.3 phase 3: derive from `ModelTabs`.
    let drilled_in_class: Option<String> = model_tabs.drilled_class_for_doc(doc);
    // Class resolution priority:
    //   1. explicit_class on the event       — API caller knows exactly
    //   2. drilled_in_class                  — UI drill-in pin
    //   3. picker modal                      — GUI fallback for ambiguity
    //   4. detected_name from AST            — single-class case
    //
    // The explicit-class branch (added in spec 033 P0) lets API/agent
    // callers compile a chosen class without ever opening the picker
    // modal. Validates against the candidate list so a bad class name
    // surfaces as a structured error in the diagnostics log instead
    // of silently picking the wrong thing.
    let chosen_via_explicit = if let Some(cls) = explicit_class.as_ref() {
        let candidates = &candidate_classes;
        // Match by short name OR full qualified name, so callers can pass
        // either `"RocketStage"` or `"AnnotatedRocketStage.RocketStage"`.
        let matched = candidates.iter().find(|c| {
            c.as_str() == cls.as_str() || c.rsplit('.').next() == Some(cls.as_str())
        });
        match matched {
            Some(qname) => Some(qname.clone()),
            None => {
                let msg = format!(
                    "compile_model class `{cls}` not found. Candidates: [{}]",
                    candidates.join(", ")
                );
                // B.3 phase 4: per-doc error.
                compile_states.set_error(doc, msg.clone());
                console.error(format!("Compile failed: {msg}"));
                let _ = diagnostics;
                return;
            }
        }
    } else {
        None
    };

    // If no explicit class and no drill-in pin and the file is a package
    // of several models, ask the user which one to compile instead of
    // silently picking. The picker modal (rendered by
    // `render_compile_class_picker` in ui/mod.rs) re-dispatches
    // `CompileModel` once the user confirms.
    if chosen_via_explicit.is_none() && drilled_in_class.is_none() {
        if candidate_classes.len() >= 2 {
            // If a picker is already open for *this* doc, leave it
            // alone so rapid repeated Compile clicks don't blow away
            // the user's in-progress choice.
            if picker.0.as_ref().map(|p| p.doc) != Some(doc) {
                picker.0 = Some(CompileClassPickerEntry {
                    doc,
                    candidates: candidate_classes.clone(),
                    preselected: 0,
                    purpose: PickerPurpose::Compile,
                });
            }
            return;
        }
    }
    let model_name = chosen_via_explicit
        .or(drilled_in_class)
        .or(detected_first_class);
    let Some(model_name) = model_name else {
        let msg = "Could not find a valid model declaration.".to_string();
        // B.3 phase 4: per-doc error.
        compile_states.set_error(doc, msg.clone());
        console.error(format!("Compile failed: {msg}"));
        return;
    };
    // Find or spawn the entity linked to this document.
    let linked = registry.entities_linked_to(doc);

    let target_entity = if let Some(&entity) = linked.first() {
        // Update existing entity in place.
        if let Ok(mut model) = q_models.get_mut(entity) {
            let old_inputs = std::mem::take(&mut model.inputs);
            model.session_id += 1;
            // `is_stepping` fences out any in-flight Step results
            // bearing the old session_id; `is_compiling` tells
            // `spawn_modelica_requests` that the wait is a normal
            // long compile (not a hung worker) — suppresses the
            // per-frame "worker hung?" warning spam during multi-
            // second Modelica compiles.
            model.is_stepping = true;
            model.is_compiling = true;
            model.model_name = model_name.clone();
            model.parameters = params.clone();
            model.inputs.clear();
            for (name, val) in &inputs_with_defaults {
                let existing = old_inputs.get(name).copied();
                model
                    .inputs
                    .entry(name.clone())
                    .or_insert_with(|| existing.unwrap_or(*val));
            }
            for name in &runtime_inputs {
                let existing = old_inputs.get(name).copied();
                model
                    .inputs
                    .entry(name.clone())
                    .or_insert_with(|| existing.unwrap_or(0.0));
            }
            model.variables.clear();
            model.paused = false;
            model.current_time = 0.0;
            model.last_step_time = 0.0;
        }
        entity
    } else {
        // No entity yet — spawn one linked to this doc. Spawning goes
        // through `Commands` (deferred), so we can't immediately
        // query the new entity in this system — initial fields are
        // set on the component at spawn time instead.
        // Initial session_id for newly-spawned model entity. Existing
        // entities bump their own `session_id` on recompile (see
        // the "updated-in-place" branch above); this starting value
        // matters only for the very first compile of a doc, after
        // which the per-entity counter takes over.
        let session_id: u64 = 1;
        let entity = commands
            .spawn((
                Name::new(model_name.clone()),
                ModelicaModel {
                    model_path: "".into(),
                    model_name: model_name.clone(),
                    current_time: 0.0,
                    last_step_time: 0.0,
                    session_id,
                    paused: false,
                    parameters: params,
                    inputs: runtime_inputs.into_iter().map(|n| (n, 0.0)).collect(),
                    variables: HashMap::new(),
                    document: doc,
                    is_stepping: true,
                    is_compiling: true,
                    is_compiled: false,
                },
            ))
            .id();
        registry.link(entity, doc);
        // Intentionally NOT setting `workbench.selected_entity` here.
        // Side panels resolve their target entity via
        // `active_simulator(world)` (= active doc → linked entity),
        // so a fresh compile on an inactive tab no longer steals the
        // visible selection from the focused tab. `selected_entity`
        // is reserved for an explicit "Pin to model" UX.
        let _ = &workbench;
        entity
    };

    // Resolve the session_id for the command we're about to send. For
    // the updated-in-place branch this is whatever we just bumped to;
    // for the newly-spawned branch the entity doesn't exist yet (spawn
    // is deferred), so fall back to the same `1` we set above.
    let session_id = q_models
        .get(target_entity)
        .map(|m| m.session_id)
        .unwrap_or(1);

    compile_states.mark_started(doc);
    console.info(format!("⏵ Compile started: '{model_name}'"));
    if let Some(diag) = diagnostics.as_mut() {
        diag.append(vec![crate::ui::panels::log::LogEntry {
            at: web_time::Instant::now(),
            level: crate::ui::panels::log::LogLevel::Info,
            text: format!("⏵ Compile started: '{model_name}'"),
            model: Some(model_name.clone()),
        }]);
    }

    if let Some(channels) = channels {
        // Get-or-create the sim stream for this entity. Cloned Arc
        // goes to the worker (owner-of-writes); the registry holds
        // the same Arc so plot panels / telemetry can read via
        // `ArcSwap::load()` on the UI thread without locking.
        let stream = sim_streams.get_or_insert(target_entity);
        // Collect sources from EVERY OTHER open Modelica doc and
        // hand them to the worker so rumoca's resolver can satisfy
        // cross-doc class references (e.g. an untitled `RocketStage`
        // referencing `AnnotatedRocketStage.Tank` from a sibling
        // untitled package). Filenames are derived from each doc's
        // origin; rumoca dedups by filename so the worker overlaying
        // the primary source as `model.mo` later is harmless.
        let extra_sources: Vec<(String, String)> = registry
            .iter()
            .filter_map(|(other_doc, host)| {
                if other_doc == doc {
                    return None;
                }
                let document = host.document();
                let filename = format!("doc_{}.mo", other_doc.raw());
                Some((filename, document.source().to_string()))
            })
            .collect();
        let _ = channels.tx.send(ModelicaCommand::Compile {
            entity: target_entity,
            session_id,
            model_name,
            source,
            extra_sources,
            stream: Some(stream),
        });
    } else {
        console.error("Modelica worker channel not available — compile dispatch dropped.");
    }
}

#[on_command(CreateNewScratchModel)]
fn on_create_new_scratch_model(
    _trigger: On<CreateNewScratchModel>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut cache: ResMut<crate::ui::panels::package_browser::PackageTreeCache>,
    mut model_tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut workbench: ResMut<WorkbenchState>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
    mut commands: Commands,
) {
    // Find the lowest `Untitled<N>` not already taken — matches VS
    // Code's `Untitled-1`, `Untitled-2` … semantics.
    let taken: std::collections::HashSet<String> = cache
        .in_memory_models
        .iter()
        .map(|e| e.display_name.clone())
        .collect();
    let mut n: u32 = 1;
    let name = loop {
        let candidate = format!("Untitled{n}");
        if !taken.contains(&candidate) {
            break candidate;
        }
        n += 1;
    };

    // Minimal parse-clean template. We previously emitted a default
    // `annotation(Icon(...));` (OMEdit-style) so a fresh class showed
    // a coloured rectangle on the canvas — but a *trailing* class-level
    // annotation poisons the pretty-printer's insertion point: every
    // subsequent `AddComponent` lands AFTER the annotation, which
    // violates the Modelica grammar (annotation must be the last
    // element in a class) and the strict parser then rejects the
    // whole document.
    //
    // TODO(scratch-icon): re-introduce the default Icon annotation by
    // wiring `SetClassAnnotation` (not yet implemented) so the icon
    // can be added through the same insertion machinery that respects
    // ordering, instead of being baked into the template text.
    let source = format!("model {name}\nend {name};\n");
    let mem_id = format!("mem://{name}");
    let doc_id = registry.allocate_with_origin(
        source.clone(),
        lunco_doc::DocumentOrigin::untitled(name.clone()),
    );

    cache.in_memory_models.retain(|e| e.id != mem_id);
    cache
        .in_memory_models
        .push(crate::ui::panels::package_browser::InMemoryEntry {
            display_name: name.clone(),
            id: mem_id.clone(),
            doc: doc_id,
        });

    let source_arc: std::sync::Arc<str> = source.into();
    workbench.editor_buffer = source_arc.to_string();
    workbench.diagram_dirty = true;

    // Sync into the Workspace session. The sync observer adds the
    // DocumentEntry on its own; what we need here is the
    // "active-document" pointer.
    workspace.active_document = Some(doc_id);

    let tab_id = model_tabs.ensure_for(doc_id, None);
    commands.trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: tab_id,
    });
}

#[on_command(DuplicateModelFromReadOnly)]
fn on_duplicate_model_from_read_only(
    trigger: On<DuplicateModelFromReadOnly>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut cache: ResMut<crate::ui::panels::package_browser::PackageTreeCache>,
    mut model_tabs: ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut duplicate_loads: ResMut<
        crate::ui::panels::canvas_diagram::DuplicateLoads,
    >,
    mut console: ResMut<crate::ui::panels::console::ConsoleLog>,
    mut commands: Commands,
    mut egui_q: Query<&mut bevy_egui::EguiContext>,
) {
    let source_doc = trigger.event().source_doc;

    // UI-thread work only: cheap lookups (registry host, string
    // clones, name collision scan). All heavy work — source text
    // extraction via regex, rewriting, and especially the rumoca
    // parse in `ModelicaDocument::with_origin` — goes to a bg task
    // below. Per the architectural rule: no O(source_bytes) work
    // on the UI thread.
    // Duplicate scope = the *file*, not the currently-drilled-in
    // inner class. Picking the AST's top-level class (the package
    // or model that owns the file) keeps cross-class refs intact —
    // an inner `model RocketStage` referencing sibling `Tank` /
    // `Valve` / `FluidPort_a` connectors stays consistent in the
    // copy, instead of being torn out into a dangling stub. The
    // user's drill-in is preserved as a navigation hint via
    // `inner_drill` so the new tab opens on the same inner class
    // they had selected.
    let (source_full, origin_class_short, origin_fqn, class_byte_range, inner_drill) = {
        let Some(host) = registry.host(source_doc) else {
            console.error("Duplicate failed: source doc not found in registry");
            return;
        };
        let doc = host.document();
        // B.3 phase 3: derive from `ModelTabs`.
        let fqn = model_tabs.drilled_class_for_doc(source_doc);
        let ast_opt = doc.strict_ast();
        // Top-level class name = first key in `ast.classes` if we
        // have a parsed AST, otherwise fall back to the origin's
        // display name (e.g. `Untitled1`). This is the *outermost*
        // declaration in the file, never an inner class.
        let top_short = ast_opt
            .as_ref()
            .and_then(|ast| ast.classes.iter().next().map(|(n, _)| n.clone()))
            .or_else(|| {
                fqn.as_ref()
                    .and_then(|q| q.split('.').next().map(String::from))
            })
            .unwrap_or_else(|| doc.origin().display_name());
        let byte_range: Option<(usize, usize)> = ast_opt
            .as_ref()
            .and_then(|ast| find_class_byte_range(ast, &top_short));
        // Path *within* the package the user was drilled into.
        // `Modelica.Foo.Pkg.RocketStage` with top `Pkg` → drill =
        // `RocketStage`. Empty / None when the user was on the top
        // class itself (drill = top short, or no drill at all).
        let inner_drill: Option<String> = fqn.as_ref().and_then(|q| {
            let suffix = q.rsplit('.').next().unwrap_or("");
            (suffix != top_short).then(|| {
                // Strip the `<within>.<top>.` prefix if present, else
                // just the `<top>.` prefix. Whatever's left is the
                // inner-class path the canvas should drill into
                // after the duplicate lands.
                let after_top = q
                    .split('.')
                    .skip_while(|seg| *seg != top_short)
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(".");
                after_top
            }).filter(|s| !s.is_empty())
        });
        (doc.source().to_string(), top_short, fqn, byte_range, inner_drill)
    };

    // Pick a new Untitled name. Try `<ClassName>Copy` first; fall
    // back to `<ClassName>CopyN` on collision.
    let taken: std::collections::HashSet<String> = cache
        .in_memory_models
        .iter()
        .map(|e| e.display_name.clone())
        .collect();
    let base_name = format!("{origin_class_short}Copy");
    let mut name = base_name.clone();
    let mut n: u32 = 2;
    while taken.contains(&name) {
        name = format!("{base_name}{n}");
        n += 1;
    }

    // Reserve a doc id. No parse, no allocation of a Document —
    // the document is built on the bg task and installed via
    // `install_prebuilt` when ready.
    let doc_id = registry.reserve_id();

    // Register the tab immediately so the user sees a new tab
    // appear in the dock even though content is still being
    // prepared. The drive system fills in the doc when the
    // bg task completes; until then the canvas overlay shows
    // "Loading resource..." for the display name.
    let mem_id = format!("mem://{name}");
    cache.in_memory_models.retain(|e| e.id != mem_id);
    cache
        .in_memory_models
        .push(crate::ui::panels::package_browser::InMemoryEntry {
            display_name: name.clone(),
            id: mem_id,
            doc: doc_id,
        });
    let tab_id = model_tabs.ensure_for(doc_id, None);
    // Duplicated copies land in Canvas view — the whole point of
    // "make a playable copy of an MSL example" is to see the
    // diagram. Text view for editing is one toolbar click away.
    if let Some(tab) = model_tabs.get_mut(tab_id) {
        tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
    }
    commands.trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: tab_id,
    });

    // Spawn the heavy work off-thread. Task captures owned data
    // only; no world access from the task.
    let origin_short_for_task = origin_class_short.clone();
    let name_for_task = name.clone();
    let origin_fqn_for_task = origin_fqn.clone();
    let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
        // We always duplicate the file-level *top* class (see the
        // `top_short` derivation above). For a single-file package
        // that means the whole source — no byte-range extraction
        // needed, and any inner classes (RocketStage, FluidPort_a,
        // …) come along automatically. Byte-range extraction was
        // only needed back when duplicate operated on the
        // currently-drilled-in inner class; under the present
        // design the AST's recovered location.end can be wrong
        // after a lenient-parse recovery, which truncated the
        // extracted source mid-package and produced a malformed
        // copy. Passing the whole source avoids that class entirely.
        let class_src = source_full;
        // 2 + 2b. Single-pass rewrite via cached spans: parse once
        //    inline, splice rename + within strip + import inject in
        //    one operation. Replaces the legacy
        //    `rewrite_duplicated_source` + `inject_class_imports`
        //    pair (each ran its own parse).
        let imports = origin_fqn_for_task
            .as_deref()
            .and_then(crate::library_fs::resolve_class_path_indexed)
            .map(|p| collect_parent_imports(&p))
            .unwrap_or_default();
        let renamed = match extract_class_spans_inline(&class_src, &origin_short_for_task) {
            Some(spans) => {
                rewrite_inject_in_one_pass(&class_src, &name_for_task, &imports, &spans)
                    .unwrap_or_else(|| class_src.clone())
            }
            None => class_src.clone(),
        };
        // 2c. Re-attach a `within <origin package>;` so within-
        //     relative type references in the copy (e.g. PID's
        //     `Blocks.Math.Gain` which is short for
        //     `Modelica.Blocks.Math.Gain`) keep resolving via the
        //     projector's scope-chain fallback. The copy's class name
        //     is new (PIDCopy), so this doesn't collide with the
        //     original. No-op when the origin FQN is unknown
        //     (non-MSL source).
        let copy_src = match origin_fqn_for_task.as_deref() {
            Some(fqn) => {
                let mut parts: Vec<&str> = fqn.split('.').collect();
                parts.pop();
                let origin_pkg = parts.join(".");
                if origin_pkg.is_empty() {
                    renamed
                } else {
                    format!("within {origin_pkg};\n{renamed}")
                }
            }
            None => renamed,
        };
        // 3. Build the document. `with_origin` runs rumoca to
        //    populate the AST cache — bg thread, so the UI stays
        //    responsive even on multi-KB sources.
        crate::document::ModelicaDocument::with_origin(
            doc_id,
            copy_src,
            lunco_doc::DocumentOrigin::untitled(name_for_task),
        )
    });

    duplicate_loads.insert(
        doc_id,
        crate::ui::panels::canvas_diagram::DuplicateBinding {
            display_name: name.clone(),
            origin_short: origin_class_short.clone(),
            inner_drill: inner_drill.clone(),
            started: web_time::Instant::now(),
            task,
        },
    );
    console.info(format!(
        "📄 Duplicating `{origin_class_short}` → `{name}` (building…)"
    ));
    // Kick the first repaint so egui actually paints the loading
    // overlay. Without this, when this command arrives via the API
    // (no keyboard/mouse input), Bevy's reactive update mode stays
    // asleep until something else pokes it — the tab opens, the bg
    // task runs invisibly, then the install lands and the user sees
    // the canvas pop straight to "done" with no loading feedback.
    // `drive_duplicate_loads` keeps the cycle alive each tick after
    // this initial kick.
    for mut ctx in egui_q.iter_mut() {
        ctx.get_mut().request_repaint();
    }
}

/// World-mut helper invoked from the `OpenClass { action: Duplicate }`
/// branch. Reserves an Untitled doc id, opens its tab in Canvas
/// view, and spawns the bg task that produces the renamed copy.
fn spawn_duplicate_class_task(world: &mut World, qualified: String, name_hint: String) {
    let origin_short = qualified
        .rsplit('.')
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| qualified.clone());

    // Resolve the requested name against existing Untitled tabs:
    // empty → derive `<short>Copy`; non-empty → use as-is, then
    // bump with a numeric suffix on collision.
    let taken: std::collections::HashSet<String> = world
        .resource::<crate::ui::panels::package_browser::PackageTreeCache>()
        .in_memory_models
        .iter()
        .map(|e| e.display_name.clone())
        .collect();
    let base_name = if name_hint.is_empty() {
        format!("{origin_short}Copy")
    } else {
        name_hint
    };
    let mut name = base_name.clone();
    let mut n: u32 = 2;
    while taken.contains(&name) {
        name = format!("{base_name}{n}");
        n += 1;
    }

    // Reserve id + open the tab now so the user sees immediate
    // feedback; the canvas will show "Loading resource…" until
    // the bg build lands via `drive_duplicate_loads`.
    let doc_id = world
        .resource_mut::<ModelicaDocumentRegistry>()
        .reserve_id();
    let mem_id = format!("mem://{name}");
    {
        let mut cache = world
            .resource_mut::<crate::ui::panels::package_browser::PackageTreeCache>();
        cache.in_memory_models.retain(|e| e.id != mem_id);
        cache
            .in_memory_models
            .push(crate::ui::panels::package_browser::InMemoryEntry {
                display_name: name.clone(),
                id: mem_id,
                doc: doc_id,
            });
    }
    // Examples are composed models — land in Canvas view so users
    // see the diagram on open, not the raw source.
    let tab_id = {
        let mut model_tabs = world
            .resource_mut::<crate::ui::panels::model_view::ModelTabs>();
        let tab_id = model_tabs.ensure_for(doc_id, None);
        if let Some(tab) = model_tabs.get_mut(tab_id) {
            tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        }
        tab_id
    };
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: tab_id,
    });

    // Bg task: resolve path → read file → extract target class →
    // rewrite → build `ModelicaDocument`. All off UI thread.
    let qualified_for_task = qualified.clone();
    let origin_short_for_task = origin_short.clone();
    let name_for_task = name.clone();
    let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
        let t_total = web_time::Instant::now();
        let t_resolve = web_time::Instant::now();
        // 1. Resolve MSL file path (static HashMap probe). If the
        //    class isn't indexed, build an empty doc so the user
        //    still gets a tab with a clear error marker.
        let Some(path) = crate::library_fs::resolve_class_path_indexed(&qualified_for_task) else {
            return crate::document::ModelicaDocument::with_origin(
                doc_id,
                format!("// Could not locate MSL file for {qualified_for_task}\n"),
                lunco_doc::DocumentOrigin::untitled(name_for_task),
            );
        };
        let resolve_ms = t_resolve.elapsed().as_secs_f64() * 1000.0;
        // 2. Read file via the unified MSL source — handles both the
        //    native filesystem (`MslAssetSource::Filesystem`) and the
        //    wasm in-memory bundle (`MslAssetSource::InMemory`). Going
        //    through `std::fs::read_to_string` directly would panic on
        //    wasm32-unknown-unknown when `path` is a relative
        //    in-memory key (`Modelica/Blocks/package.mo`) because the
        //    libstd resolver calls `current_dir()` which is fatal
        //    there ("no filesystem on this platform").
        let t_read = web_time::Instant::now();
        let source_full = lunco_assets::msl::global_msl_source()
            .and_then(|s| s.read(&path))
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        let read_ms = t_read.elapsed().as_secs_f64() * 1000.0;
        let source_len = source_full.len();
        // 3. Extract just the target class. We mask out string
        //    literals and comments first, then run the line-anchored
        //    class-header regex on the masked copy. Without masking,
        //    the regex mis-fires when an earlier class's docstring
        //    contains a literal `block <Name>` line (LimPID's canonical
        //    failure: `PID`'s docstring text "block LimPID." matches
        //    the header pattern). Masking keeps byte offsets stable so
        //    the slice into `source_full` is exact.
        // Path-aware extract: parses `path` via rumoca's
        // content-hash artifact cache (instant on repeat opens).
        // Replaces the previous double-parse via `parse_to_ast` on
        // the masked + raw source — that was 30–60s per call in
        // dev builds for 440 KB MSL package files. The masking
        // step (which prevented regex misfires inside docstrings)
        // is irrelevant once we use the typed AST: the AST already
        // distinguishes class headers from string literals.
        let t_extract = web_time::Instant::now();
        let spans_opt = extract_class_spans_via_path(
            &path,
            &source_full,
            &origin_short_for_task,
        )
        .filter(|s| s.full_start < s.full_end && s.full_end <= source_full.len());
        let class_src = match spans_opt.as_ref() {
            Some(s) => source_full[s.full_start..s.full_end].to_string(),
            None => source_full.clone(),
        };
        let extract_ms = t_extract.elapsed().as_secs_f64() * 1000.0;
        // 4 + 4b. Single-pass rewrite: parse class_src once, then
        //    rename + strip within + inject imports in one span splice.
        //    Replaces the previous two-parse pair (rewrite_duplicated_source
        //    + inject_class_imports) which each ran `parse_to_ast` on
        //    the same bytes.
        let t_collect = web_time::Instant::now();
        let imports = collect_parent_imports(&path);
        let collect_ms = t_collect.elapsed().as_secs_f64() * 1000.0;
        let t_rewrite_only = web_time::Instant::now();
        let renamed = match spans_opt.as_ref() {
            Some(spans) => rewrite_inject_in_one_pass(
                &class_src,
                &name_for_task,
                &imports,
                spans,
            )
            .unwrap_or_else(|| class_src.clone()),
            None => {
                // Path-based extract failed; try in-memory parse on
                // the (possibly whole-file) class_src as a final
                // fallback before giving up.
                extract_class_spans_inline(&class_src, &origin_short_for_task)
                    .and_then(|spans| {
                        rewrite_inject_in_one_pass(
                            &class_src,
                            &name_for_task,
                            &imports,
                            &spans,
                        )
                    })
                    .unwrap_or_else(|| class_src.clone())
            }
        };
        let rewrite_only_ms = t_rewrite_only.elapsed().as_secs_f64() * 1000.0;
        let inject_ms = 0.0_f64;
        // 4c. Re-attach a `within <origin package>;` clause so the
        //     copy's enclosing-package context is preserved for
        //     scope-chain resolution of bare `extends` refs. The
        //     origin package is `qualified` minus its leaf; falling
        //     back to an empty (unqualified) `within` if the class
        //     was top-level.
        let origin_pkg: String = {
            let mut parts: Vec<&str> = qualified_for_task.split('.').collect();
            parts.pop();
            parts.join(".")
        };
        let copy_src = if origin_pkg.is_empty() {
            renamed
        } else {
            format!("within {origin_pkg};\n{renamed}")
        };
        let rewrite_ms = rewrite_only_ms + collect_ms + inject_ms;
        // 5. Build doc lazily — no parse on the bg thread. The engine
        //    async sync (drive_engine_sync drain step) parses this
        //    on the next idle tick and backfills the doc's syntax
        //    cache. Tab-visible time = read + splice (no parse).
        let t_parse = web_time::Instant::now();
        let doc = crate::document::ModelicaDocument::with_origin(
            doc_id,
            copy_src,
            lunco_doc::DocumentOrigin::untitled(name_for_task),
        );
        let parse_ms = t_parse.elapsed().as_secs_f64() * 1000.0;
        let total_ms = t_total.elapsed().as_secs_f64() * 1000.0;
        bevy::log::info!(
            "[OpenExample bg] {qualified_for_task} src={source_len}B \
             total={total_ms:.1}ms resolve={resolve_ms:.1} read={read_ms:.1} \
             extract={extract_ms:.1} rewrite_only={rewrite_only_ms:.1} \
             collect_imports={collect_ms:.1} inject={inject_ms:.1} parse={parse_ms:.1}"
        );
        doc
    });

    world
        .resource_mut::<crate::ui::panels::canvas_diagram::DuplicateLoads>()
        .insert(
            doc_id,
            crate::ui::panels::canvas_diagram::DuplicateBinding {
                display_name: name.clone(),
                origin_short: origin_short.clone(),
                // Duplicate flow operates on a single named class
                // (`qualified` is already the leaf the user clicked) —
                // there's no inner-drill to preserve.
                inner_drill: None,
                started: web_time::Instant::now(),
                task,
            },
        );
    world
        .resource_mut::<crate::ui::panels::console::ConsoleLog>()
        .info(format!(
            "📄 Opening class `{qualified}` → editable `{name}` (building…)"
        ));
}

/// Pull the source text for a named class out of a (possibly
/// multi-class) `.mo` file. Scans for `^\s*(model|block|class|
/// connector|function|record|package|type)\s+<Name>\b` as the
/// opener and `^\s*end\s+<Name>\s*;` as the matching closer.
///
/// Works for the common MSL shapes (own-file class; single target
/// class inside a package file with no shadowing nested class of
/// the same name). Returns `None` if the opener or closer can't be
/// found — caller should fall back to copying the whole source.
/// Look up a class's `(start, end)` byte range in the source from the
/// parsed AST. Walks `ast.classes` recursively (top-level packages
/// often contain the class we're after as a nested entry, e.g.
/// `Modelica.Blocks.Continuous` → `LimPID`). The match is by short
/// name — first hit wins, which is fine in practice since MSL keeps
/// short names unique within a package.
///
/// Replaces `extract_class_source`'s regex-on-text approach. The
/// regex form mis-extracts when the source contains an earlier
/// docstring that includes a string like `block LimPID.` — the regex
/// has no notion of string-literal context and treats the docstring
/// line as the class header.
fn find_class_byte_range(
    ast: &rumoca_session::parsing::ast::StoredDefinition,
    short_name: &str,
) -> Option<(usize, usize)> {
    fn walk(
        classes: &indexmap::IndexMap<String, rumoca_session::parsing::ast::ClassDef>,
        target: &str,
    ) -> Option<(usize, usize)> {
        for (name, class) in classes.iter() {
            if name == target {
                return Some((class.location.start as usize, class.location.end as usize));
            }
            if let Some(hit) = walk(&class.classes, target) {
                return Some(hit);
            }
        }
        None
    }
    walk(&ast.classes, short_name)
}



/// Path-aware variant that also returns the class-name-token span and
/// the end-token span (both **absolute** in `source`), so the bg
/// duplicate flow can splice without re-parsing the same bytes a
/// second time. Replaces the prior pattern of calling this *plus*
/// `parse_to_ast(class_src)` inside `rewrite_inject_in_one_pass`.
fn extract_class_spans_via_path(
    path: &std::path::Path,
    source: &str,
    class_name: &str,
) -> Option<DuplicateExtract> {
    // `parse_files_parallel` resolves a per-file artifact cache rooted
    // under `std::env::temp_dir()`, which on wasm32-unknown-unknown
    // panics with "no filesystem on this platform" — `temp_dir()`'s
    // libstd stub is fatal there. On wasm we already have the source
    // bytes in memory (caller fetched them from the in-memory MSL
    // bundle), so the cache buys us nothing; parse the in-memory
    // source directly via `parse_to_ast`, same `StoredDefinition`,
    // no fs touch.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        return extract_class_spans_inline(source, class_name);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut parsed =
            rumoca_session::parsing::parse_files_parallel(&[path.to_path_buf()]).ok()?;
        let (_uri, ast) = parsed.drain(..).next()?;
        spans_from_ast(&ast, source, class_name)
    }
}

/// In-memory variant: parses `source` directly (no path / cache) and
/// returns the splice spans needed by `rewrite_inject_in_one_pass`.
/// Use when the caller has source text but no on-disk URI — e.g.,
/// duplicating a workspace doc whose source lives in
/// `ModelicaDocumentRegistry`.
fn extract_class_spans_inline(source: &str, class_name: &str) -> Option<DuplicateExtract> {
    let ast = rumoca_phase_parse::parse_to_ast(source, "duplicate-inline.mo").ok()?;
    spans_from_ast(&ast, source, class_name)
}

fn spans_from_ast(
    ast: &rumoca_session::parsing::ast::StoredDefinition,
    source: &str,
    class_name: &str,
) -> Option<DuplicateExtract> {
    let class = find_top_or_nested_class_by_short_name(ast, class_name)?;
    let (full_start, full_end) = class.full_span_with_leading_comments(source)?;
    let end_tok = class.end_name_token.as_ref()?;
    Some(DuplicateExtract {
        full_start,
        full_end,
        name_start: class.name.location.start as usize,
        name_end: class.name.location.end as usize,
        end_start: end_tok.location.start as usize,
        end_end: end_tok.location.end as usize,
    })
}

#[derive(Debug, Clone, Copy)]
struct DuplicateExtract {
    /// Class slice within the source (full_span_with_leading_comments).
    full_start: usize,
    full_end: usize,
    /// Class-name-token span (absolute in source).
    name_start: usize,
    name_end: usize,
    /// `end Name` token span (absolute in source).
    end_start: usize,
    end_end: usize,
}


/// Find a class by simple name anywhere in the parsed AST — first at
/// the top level, then walking nested-class trees.
fn find_top_or_nested_class_by_short_name<'a>(
    ast: &'a rumoca_session::parsing::ast::StoredDefinition,
    short: &str,
) -> Option<&'a rumoca_session::parsing::ast::ClassDef> {
    if let Some(class) = ast.classes.get(short) {
        return Some(class);
    }
    for class in ast.classes.values() {
        if let Some(found) = find_nested_by_short_name(class, short) {
            return Some(found);
        }
    }
    None
}

fn find_nested_by_short_name<'a>(
    class: &'a rumoca_session::parsing::ast::ClassDef,
    short: &str,
) -> Option<&'a rumoca_session::parsing::ast::ClassDef> {
    if let Some(child) = class.classes.get(short) {
        return Some(child);
    }
    for nested in class.classes.values() {
        if let Some(found) = find_nested_by_short_name(nested, short) {
            return Some(found);
        }
    }
    None
}


/// Walk from a class file's directory up through the filesystem,
/// collecting `import` statements from every `package.mo` on the
/// way. These are the imports that were in scope for the class at
/// its original location — once the class is extracted into a
/// standalone workspace file, it loses that scope, so the imports
/// must be injected into the class body itself (Modelica allows
/// class-local imports).
///
/// Stops walking as soon as a directory has no `package.mo` — that
/// marks the boundary of the enclosing package hierarchy. Returns
/// imports in outer-to-inner order, deduplicated while preserving
/// first-seen position.
///
/// Covers the SI/unit shortcuts that break duplication of MSL
/// examples: e.g. `Modelica/Blocks/package.mo` declares
/// `import Modelica.Units.SI;` which is why `SI.Angle` resolves
/// inside `Modelica.Blocks.Examples.PID_Controller` but not in a
/// naïvely extracted copy.
fn collect_parent_imports(class_file: &std::path::Path) -> Vec<String> {
    // Wasm has no filesystem, and the MSL bundle is pre-parsed and
    // already in `GLOBAL_PARSED_MSL` with all its imports. The
    // parent-walk + `read_to_string(<relative>)` chain panics on
    // wasm32-unknown-unknown ("no filesystem on this platform")
    // because libstd resolves relative paths through `current_dir()`.
    // No-op on web; rumoca's session-level resolver fills the same
    // role.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = class_file;
        return Vec::new();
    }
    let mut chain: Vec<String> = Vec::new();
    let mut dir = class_file.parent();
    while let Some(d) = dir {
        let pkg = d.join("package.mo");
        if !pkg.exists() {
            break;
        }
        // Parse the package.mo and walk the outer package class's
        // typed `imports` list. Nested-class imports stay scoped to
        // their own ClassDef.imports — only the package preamble's
        // imports leak into duplicated children, matching the prior
        // regex's "first opener through second opener" boundary.
        // `parse_files_parallel` hits rumoca's content-hash artifact
        // cache, so walking up a deep MSL hierarchy is cheap on
        // repeat duplications.
        let pairs = if std::env::var_os("LUNCO_NO_PARSE").is_some() {
            None
        } else {
            rumoca_session::parsing::parse_files_parallel(&[pkg.clone()]).ok()
        };
        if let Some(mut pairs) = pairs {
            // Re-read source so we can slice each import's location
            // back into its original `import ...;` text — preserves
            // alias / wildcard / selective forms verbatim.
            let src = match std::fs::read_to_string(&pkg) {
                Ok(s) => s,
                Err(_) => {
                    dir = d.parent();
                    continue;
                }
            };
            let stored = pairs.pop().map(|(_, s)| s);
            let pkg_class = stored.as_ref().and_then(|s| s.classes.values().next());
            let mut level: Vec<String> = Vec::new();
            if let Some(class) = pkg_class {
                use rumoca_session::parsing::ast::Import;
                for imp in &class.imports {
                    let loc = match imp {
                        Import::Qualified { location, .. }
                        | Import::Renamed { location, .. }
                        | Import::Unqualified { location, .. }
                        | Import::Selective { location, .. } => location,
                    };
                    let start = loc.start as usize;
                    let end = loc.end as usize;
                    let Some(slice) = src.get(start..end) else {
                        continue;
                    };
                    let mut text = slice.trim().to_string();
                    // Rumoca's import location ranges sometimes omit
                    // the trailing `;`. Normalise so the injected
                    // `import ...;` lines parse uniformly downstream.
                    if !text.ends_with(';') {
                        text.push(';');
                    }
                    level.push(text);
                }
            }
            // Level is the outer-relative-to-previous step. Prepend
            // so the final chain is outer-first, inner-last.
            let mut merged = level;
            merged.extend(chain.drain(..));
            chain = merged;
        }
        dir = d.parent();
    }
    let mut seen = std::collections::HashSet::new();
    chain.retain(|s| seen.insert(s.clone()));
    chain
}

/// One-parse rewrite: rename + within-strip + inject imports in a
/// single span splice over the original source. Replaces the prior
/// `rewrite_duplicated_source` + `inject_class_imports` pair, each of
/// which re-parsed the same bytes — measured at ~370ms each in dev
/// builds for a 7.9 KB extracted MSL class. This single pass parses
/// once and emits final text.
///
/// Returns `None` if the parse fails so the caller can fall back to
/// the source unchanged. (Unlikely — the caller's `extract_class_byte_range_via_path`
/// already parsed this same source successfully via the cached path.)
fn rewrite_inject_in_one_pass(
    src: &str,
    new_name: &str,
    imports: &[String],
    spans: &DuplicateExtract,
) -> Option<String> {
    // Spans are absolute in the original file. Re-anchor against the
    // class-only `src` slice (caller passes `source[full_start..full_end]`).
    let base = spans.full_start;
    let name_start = spans.name_start.checked_sub(base)?;
    let name_end = spans.name_end.checked_sub(base)?;
    let end_start = spans.end_start.checked_sub(base)?;
    let end_end = spans.end_end.checked_sub(base)?;
    if !(name_end <= end_start && end_end <= src.len()) {
        return None;
    }
    // Guard: every index we'll slice with must land on a UTF-8 char
    // boundary, otherwise `&src[a..b]` panics. Rumoca's spans have
    // historically been byte-correct on the source it parsed, but a
    // mismatch shows up the moment the caller's slice contains
    // multi-byte chars (e.g. `─` `►` `│` from pasted comments) — we'd
    // rather return None and let the caller keep the source unchanged
    // than abort the wasm thread.
    for &idx in &[name_start, name_end, end_start, end_end] {
        if !src.is_char_boundary(idx) {
            bevy::log::warn!(
                "[rewrite_inject_in_one_pass] span index {idx} not on char \
                 boundary in {}-byte source; skipping rewrite",
                src.len()
            );
            return None;
        }
    }

    // Class slice extracted by `full_span_with_leading_comments` does
    // not include the file-level `within` clause (within precedes the
    // first class header). Empty range.
    let (wstart, wend) = (0usize, 0usize);

    // Inject anchor: position in `src` immediately after the class
    // name's optional description string(s). Same scan
    // `inject_class_imports` did.
    let bytes = src.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut anchor = name_end;
    let mut scan = skip_ws(anchor);
    while scan < bytes.len() && bytes[scan] == b'"' {
        let mut j = scan + 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' if j + 1 < bytes.len() => j += 2,
                b'"' => {
                    j += 1;
                    break;
                }
                _ => j += 1,
            }
        }
        anchor = j;
        scan = skip_ws(j);
    }
    if anchor > end_start {
        return None;
    }
    let want_inject = !imports.is_empty();
    let inject_block: String = if want_inject {
        imports.iter().map(|i| format!("  {i}\n")).collect()
    } else {
        String::new()
    };

    let mut out = String::with_capacity(src.len() + inject_block.len() + 4);
    // Source up to within-strip start.
    out.push_str(&src[..wstart]);
    // Skip [wstart..wend) — within clause.
    out.push_str(&src[wend..name_start]);
    // Replace class name.
    out.push_str(new_name);
    // Description / whitespace between class name and inject anchor.
    out.push_str(&src[name_end..anchor]);
    if want_inject {
        let needs_leading_newline = !out.ends_with('\n');
        if needs_leading_newline {
            out.push('\n');
        }
        out.push_str(&inject_block);
    }
    // Body from inject anchor up to end-token.
    out.push_str(&src[anchor..end_start]);
    // Replace end-token name.
    out.push_str(new_name);
    // Tail.
    out.push_str(&src[end_end..]);
    Some(out)
}





// ─────────────────────────────────────────────────────────────────────────────
// API navigation observers
// ─────────────────────────────────────────────────────────────────────────────
//
// Each is a tiny, predictable translator from a reflect-registered
// event to the domain-specific action. `doc=0` means "active tab"
// across all of them (see also `AutoArrangeDiagram`). Observers can't
// take `&mut World` in Bevy 0.18, so the ones that need it defer via
// `commands.queue(|world| ...)` — same trick Auto-Arrange uses.

fn resolve_active_doc(world: &World) -> Option<DocumentId> {
    world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document)
}

#[on_command(FocusDocumentByName)]
fn on_focus_document_by_name(
    trigger: On<FocusDocumentByName>,
    mut commands: Commands,
) {
    let pattern = trigger.event().pattern.clone();
    if pattern.is_empty() {
        return;
    }
    commands.queue(move |world: &mut World| {
        // Case-insensitive substring match across the session's open
        // documents. First match wins.
        let hit = {
            let ws = world.resource::<lunco_workbench::WorkspaceResource>();
            let needle = pattern.to_lowercase();
            ws.documents()
                .iter()
                .find(|d| d.title.to_lowercase().contains(&needle))
                .map(|d| d.id)
        };
        let Some(doc) = hit else {
            bevy::log::info!(
                "[FocusDocumentByName] no tab matches '{}'",
                pattern
            );
            return;
        };
        let tab_id = world
            .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
            .ensure_for(doc, None);
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
            instance: tab_id,
        });
    });
}

#[on_command(SetViewMode)]
fn on_set_view_mode(trigger: On<SetViewMode>, mut commands: Commands) {
    let raw = trigger.event().doc;
    let mode_str = trigger.event().mode.clone();
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            return;
        };
        use crate::ui::panels::model_view::{ModelTabs, ModelViewMode};
        let new_mode = match mode_str.to_lowercase().as_str() {
            "text" | "source" => ModelViewMode::Text,
            "diagram" | "canvas" => ModelViewMode::Canvas,
            "icon" => ModelViewMode::Icon,
            "docs" | "documentation" => ModelViewMode::Docs,
            other => {
                bevy::log::warn!(
                    "[SetViewMode] unknown mode '{other}' — expected text|diagram|icon|docs"
                );
                return;
            }
        };
        if let Some(mut tabs) = world.get_resource_mut::<ModelTabs>() {
            // ModelTabs is now keyed by TabId, not DocumentId. Find
            // the first tab viewing `doc` and update it. Multi-tab
            // (split-view) callers should use a per-tab variant when
            // we expose one — for the API-driven SetViewMode there
            // is no tab disambiguator yet.
            if let Some(tab_id) = tabs.any_for_doc(doc) {
                if let Some(state) = tabs.get_mut(tab_id) {
                    state.view_mode = new_mode;
                }
            }
        }
    });
}

/// Approximate screen rect used by the API-side fit command. The
/// real canvas rect is only known at render time; picking 800×600
/// here matches the Fit-All menu button and produces a reasonable
/// zoom for API-driven workflows where the window size varies.
fn approx_screen_rect() -> lunco_canvas::Rect {
    lunco_canvas::Rect::from_min_max(
        lunco_canvas::Pos::new(0.0, 0.0),
        lunco_canvas::Pos::new(800.0, 600.0),
    )
}

#[on_command(SetZoom)]
fn on_set_zoom(trigger: On<SetZoom>, mut commands: Commands) {
    let raw = trigger.event().doc;
    let zoom = trigger.event().zoom;
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        use crate::ui::panels::canvas_diagram::CanvasDiagramState;
        let Some(mut state) = world.get_resource_mut::<CanvasDiagramState>() else {
            return;
        };
        let docstate = state.get_mut(doc);
        if zoom <= 0.0 {
            // zoom = 0 → fit-all. Keeps the API callable by scripts
            // that don't want to distinguish Fit from SetZoom.
            if let Some(bounds) = docstate.canvas.scene.bounds() {
                let sr = approx_screen_rect();
                let (c, z) = docstate.canvas.viewport.fit_values(bounds, sr, 40.0);
                docstate.canvas.viewport.set_target(c, z);
            }
        } else {
            let vp = &mut docstate.canvas.viewport;
            let c = vp.center;
            vp.set_target(c, zoom);
        }
    });
}

#[on_command(FocusComponent)]
fn on_focus_component(trigger: On<FocusComponent>, mut commands: Commands) {
    let raw = trigger.event().doc;
    let name = trigger.event().name.clone();
    let padding = if trigger.event().padding > 0.0 { trigger.event().padding } else { 0.5 };
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        use crate::ui::panels::canvas_diagram::CanvasDiagramState;
        let Some(mut state) = world.get_resource_mut::<CanvasDiagramState>() else {
            return;
        };
        let docstate = state.get_mut(doc);
        // Find the canvas node whose label matches `name`. Use the
        // labelled node's rect as the focus target.
        let target = docstate
            .canvas
            .scene
            .nodes()
            .find(|(_, n)| n.label == name)
            .map(|(_, n)| n.rect);
        let Some(rect) = target else {
            bevy::log::warn!("[FocusComponent] no node named `{}` on canvas", name);
            return;
        };
        // Centre on the rect; zoom so its longer dim takes `padding`
        // of the smaller viewport dim. Approx screen rect — same
        // helper Fit uses.
        let sr = approx_screen_rect();
        let viewport_dim = sr.width().min(sr.height());
        let world_dim = rect.width().max(rect.height()).max(1e-3);
        let zoom = (viewport_dim * padding) / world_dim;
        let centre = lunco_canvas::Pos::new(
            (rect.min.x + rect.max.x) * 0.5,
            (rect.min.y + rect.max.y) * 0.5,
        );
        docstate.canvas.viewport.set_target(centre, zoom);
    });
}

#[on_command(FitCanvas)]
fn on_fit_canvas(trigger: On<FitCanvas>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        use crate::ui::panels::canvas_diagram::CanvasDiagramState;
        let Some(mut state) = world.get_resource_mut::<CanvasDiagramState>() else {
            return;
        };
        // Defer to next render so Fit uses the canvas widget's
        // actual rect, not a hardcoded approximation. Without this
        // the observer-side fit picks zoom for an 800×600 viewport
        // even when the real one is 1700×800, leaving content
        // clipped at the top under the toolbar.
        state.get_mut(doc).pending_fit = true;
    });
}

#[on_command(OpenExample)]
fn on_open_example(
    trigger: On<OpenExample>,
    mut commands: Commands,
) {
    // Public-API alias for `OpenClass { action: Duplicate { name: "" } }`.
    // The duplicate handler derives a default name (`<short>Copy`)
    // when the requested name is empty.
    let qualified = trigger.event().qualified.clone();
    commands.trigger(OpenClass {
        qualified,
        action: ClassAction::Duplicate { name: String::new() },
    });
}

/// Open a class in a **read-only** tab — the same path the canvas's
/// double-click-to-drill-in gesture uses. Unlike [`OpenExample`] (which
/// duplicates into an editable Untitled doc), this opens the class
/// directly as an `msl://` tab for exploration. Reuses an existing
/// tab if the same class is already open.
///
/// `action` selects the open mode:
///   - `View` (default): read-only drill-in with `File { writable: false }` origin.
///   - `Duplicate { name }`: writable Untitled workspace copy with the
///     class renamed and parent-package imports inlined for scope-chain
///     resolution.
///
/// Both modes share the same prep (resolve path → parse cached →
/// extract target class via spans). Duplicate adds the rename +
/// import inject + within prefix.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, Default, bevy::reflect::Reflect)]
#[serde(tag = "kind")]
pub enum ClassAction {
    #[default]
    View,
    Duplicate {
        name: String,
    },
}

#[Command(default)]
pub struct OpenClass {
    pub qualified: String,
    #[serde(default)]
    pub action: ClassAction,
}

/// Startup system: register the Modelica URI handler with the
/// workbench. Runs once — the registry accepts `Arc<dyn UriHandler>`
/// and treats re-registrations as last-writer-wins, so re-running
/// would be harmless.
fn register_modelica_uri_handler(
    mut registry: ResMut<lunco_workbench::UriRegistry>,
) {
    registry.register(std::sync::Arc::new(
        crate::ui::uri_handler::ModelicaUriHandler,
    ));
    bevy::log::info!("[Modelica] registered modelica:// URI handler");
}

/// Startup system: force-init the `msl_component_library`
/// `OnceLock` on a background task so the first Welcome render
/// (or palette open) doesn't pay the ~2500-entry JSON parse cost
/// on the UI thread. Safe because `OnceLock::get_or_init` is
/// thread-safe and the later `msl_component_library()` call from
/// the render path just reads the already-initialised slice.
///
/// Uses Bevy's `AsyncComputeTaskPool` rather than `std::thread::spawn`
/// so it compiles for wasm32 (where OS threads are unavailable). On
/// wasm the task runs cooperatively on the main thread; on native it
/// runs on a real worker thread.
fn prewarm_msl_library() {
    bevy::tasks::AsyncComputeTaskPool::get()
        .spawn(async {
            let t0 = web_time::Instant::now();
            let n = crate::visual_diagram::msl_component_library().len();
            bevy::log::info!(
                "[MSL] prewarmed component library: {n} entries in {:?}",
                t0.elapsed()
            );
        })
        .detach();
}

#[on_command(OpenClass)]
fn on_open_class(trigger: On<OpenClass>, mut commands: Commands) {
    let ev = trigger.event();
    let qualified = ev.qualified.clone();
    let action = ev.action.clone();
    commands.queue(move |world: &mut World| match action {
        ClassAction::View => {
            crate::ui::panels::canvas_diagram::drill_into_class(world, &qualified);
        }
        ClassAction::Duplicate { name } => {
            spawn_duplicate_class_task(world, qualified, name);
        }
    });
}

/// Move a component instance to a new `(x, y)` position in Modelica
/// diagram coordinates (-100..100, +Y up). Same code path the mouse
/// drag uses — emits a `SetPlacement` op so undo/redo + source
/// rewrite work uniformly. `class` empty ⇒ active editing class on
/// the active tab.
#[Command(default)]
pub struct MoveComponent {
    pub class: String,
    pub name: String,
    pub x: f32,
    pub y: f32,
    /// Optional explicit extent. Empty (0.0, 0.0) means "preserve
    /// the existing extent" — reads it from the live scene the same
    /// way mouse-drag does.
    pub width: f32,
    pub height: f32,
}

/// Undo the most recent edit on the active document. Reflect-
/// registered so automation can drive the same undo path the
/// Ctrl+Z keybinding / toolbar arrow uses. `doc=0` ⇒ active tab.
#[Command(default)]
pub struct Undo {
    pub doc: DocumentId,
}

/// Redo the most recently undone edit. Mirror of [`Undo`].
#[Command(default)]
pub struct Redo {
    pub doc: DocumentId,
}

#[on_command(Undo)]
fn on_undo(trigger: On<Undo>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            bevy::log::warn!("[Undo] no active document");
            return;
        };
        world.commands().trigger(UndoDocument { doc });
    });
}

#[on_command(Redo)]
fn on_redo(trigger: On<Redo>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            bevy::log::warn!("[Redo] no active document");
            return;
        };
        world.commands().trigger(RedoDocument { doc });
    });
}

/// Pan the canvas viewport to centre on `(x, y)` in canvas world
/// coords (+Y down — same frame the projector emits node positions
/// in). Use it from API tests / automation to position the
/// viewport before screenshotting.
#[Command(default)]
pub struct PanCanvas {
    /// 0 ⇒ active document.
    pub doc: DocumentId,
    pub x: f32,
    pub y: f32,
}

/// Gracefully shut down the application. Exposed so automation can
/// stop the workbench without the operator having to confirm a kill
/// signal each time.
#[Command(default)]
pub struct Exit {}

/// Run rumoca-tool-fmt on the active document and replace its
/// source with the formatted text. Single undo step. No-op on
/// read-only tabs or when formatting fails (parse errors etc.).
#[Command(default)]
pub struct FormatDocument {
    /// 0 ⇒ active document.
    pub doc: DocumentId,
}

#[on_command(FormatDocument)]
fn on_format_document(trigger: On<FormatDocument>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        use crate::document::ModelicaOp;
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        let Some(doc) = doc else {
            bevy::log::warn!("[FormatDocument] no active document");
            return;
        };
        // B.3 phase 6: derive from registry.
        let workbench_read_only = crate::ui::state::read_only_for(world, doc);
        if workbench_read_only {
            bevy::log::info!("[FormatDocument] tab is read-only — skipping");
            return;
        }
        let Some(registry) = world.get_resource::<crate::ui::state::ModelicaDocumentRegistry>()
        else {
            return;
        };
        let Some(host) = registry.host(doc) else { return };
        let original = host.document().source().to_string();
        let opts = rumoca_tool_fmt::FormatOptions::default();
        let formatted = match rumoca_tool_fmt::format_with_source_name(
            &original, &opts, "<editor>",
        ) {
            Ok(s) => s,
            Err(e) => {
                bevy::log::warn!("[FormatDocument] format failed: {}", e);
                return;
            }
        };
        if formatted == original {
            return;
        }
        // Route through the document op pipeline so undo/redo +
        // canvas reprojection both work the same way as a manual
        // edit.
        let mut registry = world.resource_mut::<crate::ui::state::ModelicaDocumentRegistry>();
        if let Some(host) = registry.host_mut(doc) {
            let _ = host.apply(ModelicaOp::ReplaceSource { new: formatted });
        }
    });
}

/// Publish every Untitled (in-memory, not yet saved) Modelica
/// document into the cross-domain `UnsavedDocs` resource the Files
/// browser section reads.
///
/// **Change-driven, not per-frame.** Bevy's `Res::is_changed()` flips
/// only on the tick when something mutated the registry (allocate,
/// install_prebuilt, remove_document, set_origin, …). When neither
/// the registry nor the cross-domain resource has ticked since our
/// last write, bail without recomputing — saves walking the doc
/// list every frame for a UI surface that changes a few times per
/// session.
fn publish_unsaved_modelica_docs(
    registry: Res<crate::ui::state::ModelicaDocumentRegistry>,
    unsaved: Option<ResMut<lunco_workbench::UnsavedDocs>>,
) {
    let Some(mut unsaved) = unsaved else { return };
    if !registry.is_changed() && !unsaved.is_added() {
        return;
    }
    unsaved.entries = registry
        .iter()
        // Workspace = user content. Read-only library docs (MSL
        // classes the user clicked into) aren't part of the
        // workspace — same filter the Modelica section uses.
        .filter(|(_, host)| {
            let o = host.document().origin();
            o.is_writable() || o.is_untitled()
        })
        .map(|(_, host)| {
            let origin = host.document().origin();
            lunco_workbench::UnsavedDocEntry {
                display_name: origin.display_name(),
                kind: "Modelica".into(),
                is_unsaved: origin.is_untitled(),
            }
        })
        .collect();
}

/// Surface the active document's compile state + workspace activity
/// in the workbench status bar so users can tell at a glance what's
/// running. Reads-only — runs every frame, writes via
/// `WorkbenchLayout::set_status`.
///
/// Status priority (first-match wins):
///   1. Compile in flight on active doc → "Compiling <model>…".
///   2. Compile error on active doc → "Compile error".
///   3. Compile ready on active doc → "Compiled <model>".
///   4. No active doc → "ready".
fn update_status_bar(
    workbench: Res<crate::ui::WorkbenchState>,
    workspace: Option<Res<lunco_workbench::WorkspaceResource>>,
    compile_states: Res<crate::ui::CompileStates>,
    layout: Option<ResMut<lunco_workbench::WorkbenchLayout>>,
    registry: Res<crate::ui::state::ModelicaDocumentRegistry>,
) {
    let Some(mut layout) = layout else { return };
    // Re-render only when something a status reader cares about
    // ticked: the active document changed, the compile state
    // transitioned, the open model swapped. Cheap idle path —
    // most frames have no change.
    let any_change = workbench.is_changed()
        || compile_states.is_changed()
        || workspace.as_ref().map(|w| w.is_changed()).unwrap_or(false);
    if !any_change && !layout.is_added() {
        return;
    }
    let active_doc = workspace.as_ref().and_then(|w| w.active_document);
    // B.3 phase 6: derive from registry directly.
    let model_name = active_doc
        .and_then(|d| {
            use lunco_doc::Document as _;
            registry.host(d).and_then(|h| {
                let document = h.document();
                document
                    .strict_ast()
                    .and_then(|ast| crate::ast_extract::extract_model_name_from_ast(&ast))
                    .or_else(|| Some(document.origin().display_name().to_string()))
            })
        })
        .unwrap_or_else(|| "(untitled)".to_string());
    let _ = workbench;

    let text = match active_doc {
        None => "ready".to_string(),
        Some(doc) => match compile_states.state_of(doc) {
            crate::ui::CompileState::Compiling => format!("⏳ Compiling {model_name}…"),
            crate::ui::CompileState::Error => format!("⚠ Compile error in {model_name}"),
            crate::ui::CompileState::Ready => format!("✓ Compiled {model_name}"),
            crate::ui::CompileState::Idle => format!("● {model_name}"),
        },
    };
    layout.set_status(text);
}

/// API-accessible Save / SaveAs.
///
/// `SaveActiveDocument` writes through the existing `SaveDocument`
/// pipeline (no path picker — fails if the doc is Untitled). Use
/// `SaveActiveDocumentAs` to bind a path explicitly without the
/// modal picker; this is the form scripts and tests should use.
#[Command(default)]
pub struct SaveActiveDocument {
    /// 0 ⇒ active document.
    pub doc: DocumentId,
}

#[Command(default)]
pub struct SaveActiveDocumentAs {
    /// 0 ⇒ active document.
    pub doc: DocumentId,
    /// Target filesystem path. Bypasses the native picker so
    /// automation can save without GUI interaction.
    pub path: String,
}

#[on_command(SaveActiveDocument)]
fn on_save_active_document(trigger: On<SaveActiveDocument>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        let Some(doc) = doc else {
            bevy::log::warn!("[SaveActiveDocument] no active document");
            return;
        };
        world.commands().trigger(SaveDocument { doc });
    });
}

#[on_command(SaveActiveDocumentAs)]
fn on_save_active_document_as(
    trigger: On<SaveActiveDocumentAs>,
    mut commands: Commands,
) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        let doc = if ev.doc.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(ev.doc)
        };
        let Some(doc) = doc else {
            bevy::log::warn!("[SaveActiveDocumentAs] no active document");
            return;
        };
        let path = std::path::PathBuf::from(&ev.path);
        // Snapshot source, then write through lunco-storage and
        // rebind the doc origin to the new path — same effect as the
        // SaveAs picker path, minus the modal.
        let source = {
            let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
            let Some(host) = registry.host(doc) else { return };
            host.document().source().to_string()
        };
        if let Err(e) = std::fs::write(&path, source.as_bytes()) {
            bevy::log::warn!("[SaveActiveDocumentAs] write failed {}: {}", path.display(), e);
            return;
        }
        let mut registry = world.resource_mut::<crate::ui::state::ModelicaDocumentRegistry>();
        if let Some(host) = registry.host_mut(doc) {
            host.document_mut().set_origin(lunco_doc::DocumentOrigin::File {
                path: path.clone(),
                writable: true,
            });
        }
        registry.mark_document_saved(doc);
        bevy::log::info!(
            "[SaveActiveDocumentAs] saved {} ({} bytes)",
            path.display(),
            source.len(),
        );
        world.commands().trigger(DocumentSaved::local(doc));
    });
}

/// API shim: duplicate the active read-only document into a fresh
/// editable workspace tab. Fires the existing
/// `DuplicateModelFromReadOnly` event with `doc=0` ⇒ active.
#[Command(default)]
pub struct DuplicateActiveDoc {
    pub doc: DocumentId,
}

#[on_command(DuplicateActiveDoc)]
fn on_duplicate_active_doc(trigger: On<DuplicateActiveDoc>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        let Some(doc) = doc else {
            bevy::log::warn!("[DuplicateActiveDoc] no active document");
            return;
        };
        world.commands().trigger(DuplicateModelFromReadOnly { source_doc: doc });
    });
}

/// Run-control events — fire against `doc=0` to target the active
/// document, or a specific `DocumentId.raw()` for automation.
///
/// Simulation already ticks automatically once a model is compiled
/// (see `spawn_modelica_requests` — steps every `FixedUpdate` unless
/// `ModelicaModel.paused`). These commands are the user-facing
/// handles on that loop:
///
///  * [`PauseActiveModel`]  — freeze stepping without tearing down
///    worker state. `paused = true`.
///  * [`ResumeActiveModel`] — thaw from paused. `paused = false`.
///  * [`ResetActiveModel`]  — send `ModelicaCommand::Reset` to the
///    worker so it rebuilds the stepper from the cached DAE and
///    zeroes `current_time`. Cheap — no recompile.
///
/// A separate Step-one-frame command is intentionally deferred until
/// #59 (named experiments / Runs panel) lands — the infrastructure
/// for a "force one step" flag is better designed alongside that.
#[Command(default)]
pub struct PauseActiveModel {
    pub doc: DocumentId,
}

/// See [`PauseActiveModel`].
#[Command(default)]
pub struct ResumeActiveModel {
    pub doc: DocumentId,
}

/// See [`PauseActiveModel`].
#[Command(default)]
pub struct ResetActiveModel {
    pub doc: DocumentId,
}

#[on_command(PauseActiveModel)]
fn on_pause_active_model(trigger: On<PauseActiveModel>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            return;
        };
        if let Some(entity) = entity_for_doc(world, doc) {
            if let Some(mut model) = world.get_mut::<ModelicaModel>(entity) {
                model.paused = true;
            }
        }
    });
}

#[on_command(ResumeActiveModel)]
fn on_resume_active_model(trigger: On<ResumeActiveModel>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            return;
        };
        if let Some(entity) = entity_for_doc(world, doc) {
            if let Some(mut model) = world.get_mut::<ModelicaModel>(entity) {
                model.paused = false;
            }
        }
    });
}

/// Fast Run — compile + simulate end-to-end off-thread (Web Worker on
/// wasm, std::thread on native). The result is stored as an Experiment
/// in [`lunco_experiments::ExperimentRegistry`]. See
/// `docs/architecture/25-experiments.md`.
#[Command(default)]
pub struct FastRunActiveModel {
    pub doc: DocumentId,
}

#[on_command(FastRunActiveModel)]
fn on_fast_run_active_model(trigger: On<FastRunActiveModel>, mut commands: Commands) {
    use lunco_experiments::ExperimentRunner;
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            bevy::log::warn!("[FastRunActiveModel] no active document");
            return;
        };

        // Resolve source + target class. Mirrors `on_compile_model`
        // class resolution: drilled-in class > picker (when ambiguous)
        // > sole non-package class. Without this, package-wrapped
        // models (AnnotatedRocketStage etc.) fail with "no compilable
        // top-level class".
        let (source, filename, candidates) = {
            let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
            let host = match registry.host(doc) {
                Some(h) => h,
                None => {
                    bevy::log::warn!("[FastRunActiveModel] doc {} not in registry", doc.raw());
                    return;
                }
            };
            let document = host.document();
            let source = document.source().to_string();
            let filename = document.origin().display_name().to_string();
            let index = document.index();
            let candidates: Vec<String> = index
                .classes
                .values()
                .filter(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                .map(|c| c.name.clone())
                .collect();
            (source, filename, candidates)
        };
        let drilled =
            crate::ui::panels::model_view::drilled_class_for_doc(world, doc);
        let model_name = match drilled {
            Some(c) => c,
            None => match candidates.len() {
                0 => {
                    bevy::log::warn!(
                        "[FastRunActiveModel] doc {} has no compilable top-level class",
                        doc.raw()
                    );
                    return;
                }
                1 => candidates[0].clone(),
                _ => {
                    // Ambiguous — open the same modal Compile uses,
                    // tagged with FastRun purpose so confirmation
                    // re-dispatches FastRunActiveModel.
                    if let Some(mut picker) =
                        world.get_resource_mut::<CompileClassPickerState>()
                    {
                        if picker.0.as_ref().map(|p| p.doc) != Some(doc) {
                            picker.0 = Some(CompileClassPickerEntry {
                                doc,
                                candidates,
                                preselected: 0,
                                purpose: PickerPurpose::FastRun,
                            });
                        }
                    }
                    return;
                }
            },
        };

        let model_ref = lunco_experiments::ModelRef(model_name.clone());

        // Snapshot source into the runner so the worker thread / web
        // worker can compile without touching the live editor state.
        let runner_res = match world.get_resource::<crate::ModelicaRunnerResource>() {
            Some(r) => r.clone(),
            None => {
                bevy::log::error!("[FastRunActiveModel] runner resource missing");
                return;
            }
        };
        runner_res.0.set_model_source(
            model_ref.clone(),
            crate::experiments_runner::ModelSource {
                model_name: model_name.clone(),
                source,
                filename,
                extras: Vec::new(),
            },
        );

        // Bounds default from runner-side annotation cache (populated
        // after a successful Compile via set_model_defaults). Fallback
        // 0..1.
        let bounds = runner_res
            .0
            .default_bounds(&model_ref)
            .unwrap_or_else(|| lunco_experiments::RunBounds {
                t_start: 0.0,
                t_end: 1.0,
                dt: None,
                tolerance: None,
                solver: None,
            });

        // Pull the override + bounds draft for this model, if any.
        let (overrides, bounds) = {
            let drafts = world.resource::<crate::experiments_runner::ExperimentDrafts>();
            match drafts.get(&model_ref) {
                Some(d) => (
                    d.overrides.clone(),
                    d.bounds_override.clone().unwrap_or(bounds),
                ),
                None => (Default::default(), bounds),
            }
        };

        // Insert experiment + dispatch run.
        let twin_id = lunco_experiments::TwinId("default".into());
        let exp_id = {
            let mut reg = world.resource_mut::<lunco_experiments::ExperimentRegistry>();
            reg.insert_new(twin_id, model_ref, overrides, bounds)
        };
        let exp = world
            .resource::<lunco_experiments::ExperimentRegistry>()
            .get(exp_id)
            .cloned();
        let Some(exp) = exp else {
            bevy::log::error!("[FastRunActiveModel] experiment vanished after insert");
            return;
        };

        let handle = runner_res.0.run_fast(&exp);
        // Remember which document started this run so failures can be
        // routed back into the doc's CompileStates + Console.
        world
            .resource_mut::<crate::experiments_runner::ExperimentSources>()
            .0
            .insert(exp_id, doc);
        // Store the handle so a draining system can pump updates into
        // registry status.
        world
            .resource_mut::<crate::experiments_runner::PendingHandles>()
            .0
            .push(handle);
        bevy::log::info!(
            "[FastRunActiveModel] dispatched run {:?} for class '{}'",
            exp_id,
            model_name
        );
        if let Some(mut console) =
            world.get_resource_mut::<crate::ui::panels::console::ConsoleLog>()
        {
            console.info(format!(
                "▶ Fast Run: '{}' (t={:.2}→{:.2}s)",
                model_name, exp.bounds.t_start, exp.bounds.t_end
            ));
        }
    });
}

#[on_command(ResetActiveModel)]
fn on_reset_active_model(trigger: On<ResetActiveModel>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let Some(doc) = (if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        }) else {
            return;
        };
        let Some(entity) = entity_for_doc(world, doc) else {
            return;
        };
        // Snapshot session_id, bump it so stale Step results fence out,
        // then ship Reset to the worker.
        let session_id = {
            let Some(mut model) = world.get_mut::<ModelicaModel>(entity) else {
                return;
            };
            model.session_id += 1;
            model.is_stepping = true;
            model.current_time = 0.0;
            model.last_step_time = 0.0;
            model.variables.clear();
            model.session_id
        };
        if let Some(channels) = world.get_resource::<crate::ModelicaChannels>() {
            let _ = channels.tx.send(crate::ModelicaCommand::Reset { entity, session_id });
        }
    });
}

/// Locate the Modelica simulation entity linked to `doc`, if any.
fn entity_for_doc(world: &World, doc: DocumentId) -> Option<Entity> {
    world
        .get_resource::<ModelicaDocumentRegistry>()
        .and_then(|r| r.entities_linked_to(doc).into_iter().next())
}

/// Drop a Simulink-style "Scope" plot onto the active canvas at
/// world-space position `(x, y)` with the given size, optionally
/// bound to a scalar signal. Pure UI overlay — does not emit
/// Modelica source. Uses the active document's coordinate frame
/// (same as `MoveComponent`: -100..100 typical, +Y down).
///
/// `signal` may be empty: the plot is then created unbound (no
/// entity, no path) — matching the right-click menu's "Empty plot
/// (bind later)" entry. Useful for headless / API-driven UI tests
/// that want to verify a plot lands on the canvas before any sim
/// has run.
#[Command(default)]
pub struct AddCanvasPlot {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Signal path the plot will display, resolved against the
    /// active `ModelicaModel` entity. Empty ⇒ unbound plot.
    pub signal: String,
}

#[on_command(AddCanvasPlot)]
fn on_add_canvas_plot(trigger: On<AddCanvasPlot>, mut commands: Commands) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        use crate::document::ModelicaOp;
        let Some(doc) = resolve_active_doc(world) else {
            bevy::log::warn!("[AddCanvasPlot] no active document");
            return;
        };
        // Default plot tile size — slightly wider than tall so a
        // time-series chart reads naturally next to the typical
        // 100×100 component icon.
        let w = if ev.width > 0.0 { ev.width } else { 120.0 };
        let h = if ev.height > 0.0 { ev.height } else { 90.0 };
        if ev.signal.is_empty() {
            // No source-side persistence path for unbound plots —
            // the annotation requires a non-empty `signal=`. Refuse
            // up front so the user gets one consistent error story
            // (the inspector "bind a signal" UX is unchanged).
            bevy::log::warn!(
                "[AddCanvasPlot] empty signal — skipping (bind one first)"
            );
            return;
        }
        // Resolve the target class the same way `on_move_component`
        // does — drill-in target wins, then the workbench's
        // detected name. Empty class is a hard error: every plot
        // tile lives inside a specific class's Diagram annotation.
        // B.3: derive from `ModelTabs`.
        let class = crate::ui::panels::model_view::drilled_class_for_doc(world, doc)
            // B.3 phase 6: derive from registry.
            .or_else(|| crate::ui::state::detected_name_for(world, doc))
            .unwrap_or_default();
        if class.is_empty() {
            bevy::log::warn!("[AddCanvasPlot] could not resolve target class for doc");
            return;
        }
        let plot = crate::pretty::LunCoPlotNodeSpec {
            x1: ev.x,
            y1: ev.y,
            x2: ev.x + w,
            y2: ev.y + h,
            signal: ev.signal.clone(),
            title: String::new(),
        };
        bevy::log::info!(
            "[AddCanvasPlot] doc={} class={class} signal={} at ({},{}) {}x{}",
            doc.raw(), ev.signal, ev.x, ev.y, w, h,
        );
        crate::ui::panels::canvas_diagram::apply_ops_public(
            world,
            doc,
            vec![ModelicaOp::AddPlotNode { class, plot }],
        );
        // The reprojection that follows the source rewrite emits a
        // `lunco.viz.plot` scene Node from the new annotation, so
        // there's no optimistic `scene.insert_node` here — letting
        // the source be the single source of truth keeps add /
        // delete / undo coherent without extra plumbing.
    });
}

/// Open a new time-series plot panel (`VizPanel`) in the bottom dock.
/// Each call allocates a fresh `VizId` and inserts a `LinePlot`-kind
/// `VisualizationConfig`. The initial `signals` list (Modelica
/// dotted variable paths) is bound on creation; more can be added
/// later via [`AddSignalToPlot`].
#[Command(default)]
pub struct NewPlotPanel {
    /// Tab title. Empty ⇒ auto-named "Plot #N".
    pub title: String,
    /// Initial signals to plot. Each is a fully-qualified scalar
    /// variable path (e.g. `"P.y"`).
    pub signals: Vec<String>,
}

#[on_command(NewPlotPanel)]
fn on_new_plot_panel(trigger: On<NewPlotPanel>, mut commands: Commands) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        use lunco_viz::{
            kinds::line_plot::LINE_PLOT_KIND, view::ViewTarget, viz::SignalBinding,
            viz::VisualizationConfig, viz::VizId, SignalRef, VisualizationRegistry,
        };
        let id = VizId::next();
        let title = if ev.title.is_empty() {
            format!("Plot #{}", id.0)
        } else {
            ev.title.clone()
        };
        // Bind signals to the first ModelicaModel entity — same
        // entity Telemetry's checkbox uses. SignalRegistry is keyed
        // by (entity, path) so a signal bound to the wrong entity
        // never plots. If no model is loaded, drop binding to
        // PLACEHOLDER so the plot still opens (empty until the
        // user simulates and re-plots).
        let model_entity = world
            .query::<(bevy::prelude::Entity, &crate::ModelicaModel)>()
            .iter(world)
            .next()
            .map(|(e, _)| e);
        let inputs: Vec<SignalBinding> = ev
            .signals
            .iter()
            .map(|s| {
                let entity = model_entity.unwrap_or(bevy::prelude::Entity::PLACEHOLDER);
                SignalBinding {
                    source: SignalRef::new(entity, s.clone()),
                    role: "y".into(),
                    label: None,
                    color: None,
                    visible: true,
                }
            })
            .collect();
        let mut registry = world.resource_mut::<VisualizationRegistry>();
        registry.insert(VisualizationConfig {
            id,
            title: title.clone(),
            kind: LINE_PLOT_KIND.clone(),
            view: ViewTarget::Panel2D,
            inputs,
            style: serde_json::Value::Null,
        });
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: lunco_viz::VIZ_PANEL_KIND,
            instance: id.0,
        });
        bevy::log::info!("[NewPlotPanel] opened `{}` (id={})", title, id.0);
    });
}

/// Add one signal to an existing plot panel. `plot=0` ⇒ the
/// singleton default Modelica graph.
#[Command(default)]
pub struct AddSignalToPlot {
    pub plot: u64,
    pub signal: String,
}

#[on_command(AddSignalToPlot)]
fn on_add_signal_to_plot(trigger: On<AddSignalToPlot>, mut commands: Commands) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        use lunco_viz::{viz::SignalBinding, viz::VizId, SignalRef, VisualizationRegistry};
        let id = if ev.plot == 0 {
            crate::ui::viz::DEFAULT_MODELICA_GRAPH
        } else {
            VizId(ev.plot)
        };
        let model_entity = world
            .query::<(bevy::prelude::Entity, &crate::ModelicaModel)>()
            .iter(world)
            .next()
            .map(|(e, _)| e)
            .unwrap_or(bevy::prelude::Entity::PLACEHOLDER);
        let mut registry = world.resource_mut::<VisualizationRegistry>();
        let Some(cfg) = registry.get_mut(id) else {
            bevy::log::warn!("[AddSignalToPlot] no plot with id={}", ev.plot);
            return;
        };
        let signal_ref = SignalRef::new(model_entity, ev.signal.clone());
        if cfg.inputs.iter().any(|b| b.source == signal_ref) {
            return;
        }
        cfg.inputs.push(SignalBinding {
            source: signal_ref,
            role: "y".into(),
            label: None,
            color: None,
            visible: true,
        });
    });
}

/// API shim for `CompileModel`: same effect (rumoca compile + DAE
/// + simulator setup) but takes `doc: u64` (0 = active) so it can
/// be triggered from the reflect-registered API. Inner `CompileModel`
/// stays as a typed Bevy event for in-process callers; this exposes
/// it to curl / scripts. Type-check / parse / DAE errors land in
/// `WorkbenchState.compilation_error` which the Diagnostics panel
/// already surfaces.
#[Command(default)]
pub struct CompileActiveModel {
    /// 0 ⇒ active document.
    pub doc: DocumentId,
    /// Optional target class. Empty = inherit picker / drilled-in /
    /// detected-name behaviour. When non-empty, the compile bypasses
    /// the GUI class-picker for documents with multiple non-package
    /// classes — required for headless / agent-driven workflows where
    /// no human is available to click the modal (cf. spec 033 P0).
    /// Lookup is by short name (e.g. `"RocketStage"`) matched against
    /// the document's `collect_non_package_classes_qualified`.
    pub class: String,
}

#[on_command(CompileActiveModel)]
fn on_compile_active_model(trigger: On<CompileActiveModel>, mut commands: Commands) {
    let raw = trigger.event().doc;
    let class = trigger.event().class.clone();
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(raw)
        };
        let Some(doc) = doc else {
            bevy::log::warn!("[CompileActiveModel] no active document");
            return;
        };
        let target_class = if class.is_empty() { None } else { Some(class) };
        world.commands().trigger(CompileModel { doc, class: target_class });
    });
}

/// Inspect the active document's parsed AST and log the results
/// (top-level class names, parse error if any). API automation
/// uses this to diagnose why a drill-in or projection produced
/// zero nodes — if the AST is empty, the file failed strict parse.
#[Command(default)]
pub struct InspectActiveDoc {}

#[on_command(InspectActiveDoc)]
fn on_inspect_active_doc(_trigger: On<InspectActiveDoc>, mut commands: Commands) {
    commands.queue(|world: &mut World| {
        let doc = resolve_active_doc(world);
        let Some(doc) = doc else {
            bevy::log::warn!("[InspectActiveDoc] no active document");
            return;
        };
        let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
        let Some(host) = registry.host(doc) else {
            bevy::log::warn!("[InspectActiveDoc] doc {} not in registry", doc.raw());
            return;
        };
        let document = host.document();
        let cache = document.ast();
        let origin = document.origin();
        bevy::log::info!(
            "[InspectActiveDoc] doc={} origin={:?} source_len={} gen={}",
            doc.raw(),
            origin.display_name(),
            document.source().len(),
            cache.generation,
        );
        if cache.has_errors() {
            for e in &cache.errors {
                bevy::log::warn!("[InspectActiveDoc]   parse ERR: {}", e);
            }
        } else if let Some(ast) = document.strict_ast() {
            bevy::log::info!(
                "[InspectActiveDoc]   parse OK; within={:?}",
                ast.within.as_ref().map(|w| w.to_string()),
            );
            fn dump(
                name: &str,
                class: &rumoca_session::parsing::ast::ClassDef,
                depth: usize,
            ) {
                let indent = "  ".repeat(depth + 1);
                let comps: Vec<String> = class
                    .components
                    .iter()
                    .map(|(n, c)| format!("{}: {}", n, c.type_name))
                    .collect();
                bevy::log::info!(
                    "[InspectActiveDoc]{}{} ({:?}) extends={} components=[{}]",
                    indent,
                    name,
                    class.class_type,
                    class.extends.len(),
                    comps.join(", "),
                );
                for (cn, child) in &class.classes {
                    dump(cn, child, depth + 1);
                }
            }
            for (n, c) in &ast.classes {
                dump(n, c, 0);
            }
        } else {
            bevy::log::warn!(
                "[InspectActiveDoc]   parse cache empty — likely worker parse pending"
            );
        }
    });
}

// `OpenFile` itself now lives in `lunco-workbench` (shell-level verb,
// shared across all three apps). The Modelica-specific observer below
// reads `.mo` content into the document registry; future domains add
// their own observers and gate on file extension. The `register_commands!()`
// list in this crate keeps `on_open_file` so the type+observer pairing
// gets registered when `ModelicaCommandsPlugin` is added — `register_type`
// is idempotent so the workbench's own registration of the type is fine.
#[on_command(OpenFile)]
fn on_open_file(trigger: On<OpenFile>, mut commands: Commands) {
    let path = trigger.event().path.clone();
    commands.queue(move |world: &mut World| {
        // Scheme dispatch on the canonical openable-source URIs. Plain
        // absolute / relative paths fall through to the legacy fs-read
        // branch so existing callers (Open File dialog, drag-and-drop)
        // keep working unchanged.
        if let Some(filename) = path.strip_prefix("bundled://") {
            open_bundled_in_world(world, filename);
            return;
        }
        if let Some(name) = path.strip_prefix("mem://") {
            focus_in_memory_doc(world, name);
            return;
        }

        // Gate on `.mo` extension. Other domain crates (USD, SysML,
        // …) observe the same `OpenFile` event for their own
        // extensions; without this gate, a `.usda` Twin auto-load
        // ended up parsed here as a Modelica file and opened a junk
        // Canvas tab. The Twin browser surfaces every file kind via
        // its own section regardless of which observer claims it.
        let lower = path.to_ascii_lowercase();
        let is_modelica = std::path::Path::new(&lower)
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext == "mo")
            .unwrap_or(false);
        if !is_modelica {
            return;
        }

        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                bevy::log::warn!("[OpenFile] {} read failed: {}", path, e);
                return;
            }
        };
        let path_buf = std::path::PathBuf::from(&path);
        let stem = path_buf
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Opened")
            .to_string();
        let mut registry =
            world.resource_mut::<crate::ui::state::ModelicaDocumentRegistry>();
        let doc_id = registry.allocate_with_origin(
            source.clone(),
            lunco_doc::DocumentOrigin::File {
                path: path_buf,
                writable: true,
            },
        );
        // Land in Canvas view so the user sees the diagram.
        let mut tabs = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>();
        let tab_id = tabs.ensure_for(doc_id, None);
        if let Some(tab) = tabs.get_mut(tab_id) {
            tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        }
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
            instance: tab_id,
        });
        bevy::log::info!("[OpenFile] opened `{}` as `{}`", path, stem);
    });
}

/// Open a bundled (`assets/models/*.mo`) example as an Untitled doc.
/// Mirrors what the Welcome tab's bundled-card click path does, but
/// reachable through the API for headless / agent-driven flows. Lands
/// the doc in Canvas view to match the rest of the open-source-of-truth
/// behaviour. Untitled because bundled sources are read-only embedded
/// data — saving needs Save-As.
fn open_bundled_in_world(world: &mut World, filename: &str) {
    let Some(source) = crate::models::get_model(filename) else {
        bevy::log::warn!("[OpenFile] no bundled model named `{}`", filename);
        return;
    };
    // Strip the extension for the tab title — `RC_Circuit.mo` →
    // `RC_Circuit`. Falls back to the full filename if there is no
    // extension separator.
    let display_name = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename)
        .to_string();
    let mut registry =
        world.resource_mut::<crate::ui::state::ModelicaDocumentRegistry>();
    let doc_id = registry.allocate_with_origin(
        source.to_string(),
        lunco_doc::DocumentOrigin::untitled(display_name.clone()),
    );
    let mut tabs = world.resource_mut::<crate::ui::panels::model_view::ModelTabs>();
    let tab_id = tabs.ensure_for(doc_id, None);
    if let Some(tab) = tabs.get_mut(tab_id) {
        tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
    }
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: tab_id,
    });
    bevy::log::info!("[OpenFile] opened bundled `{}` as `{}`", filename, display_name);
}

/// Focus an already-open Untitled tab by `mem://Name`. Does **not**
/// create a doc — the URI references existing in-memory state. If no
/// tab matches the name, logs a warning and no-ops; callers should
/// `list_open_documents` first to verify.
fn focus_in_memory_doc(world: &mut World, name: &str) {
    let target_id = format!("mem://{}", name);
    let cache = world.resource::<crate::ui::panels::package_browser::PackageTreeCache>();
    let entry = cache
        .in_memory_models
        .iter()
        .find(|e| e.id == target_id)
        .map(|e| e.doc);
    drop(cache);
    let Some(doc_id) = entry else {
        bevy::log::warn!(
            "[OpenFile] no Untitled doc named `{}` (mem:// requires an existing tab)",
            name
        );
        return;
    };
    // Re-fire OpenTab — workbench treats this as "focus existing".
    let tab_id = world
        .resource_mut::<crate::ui::panels::model_view::ModelTabs>()
        .ensure_for(doc_id, None);
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: tab_id,
    });
}

/// Read a file from the filesystem and log its contents to the
/// console at INFO level. Useful for automation that wants to
/// fetch a file's content via the API without spawning a separate
/// shell. Resolves `path` relative to the workbench's current
/// working directory.
#[Command(default)]
pub struct GetFile {
    pub path: String,
}

/// Unified open command — dispatches on the URI scheme so an agent
/// (or any caller) does not need to know whether the target is bundled,
/// MSL, on disk, or already open as Untitled.
///
/// Scheme dispatch:
/// - `bundled://Filename.mo` → forward to [`OpenFile`] (which now
///   recognises the scheme and opens the embedded source as Untitled).
/// - `mem://Name` → forward to [`OpenFile`] (focuses the existing
///   Untitled tab — does not create a new doc).
/// - Dot-separated qualified Modelica name (`Modelica.Blocks.Examples.PID`)
///   → forward to [`OpenExample`].
/// - Anything else → forward to [`OpenFile`] (raw fs path).
///
/// The legacy `OpenFile` / `OpenClass` / `OpenExample` commands stay
/// available for callers that already use them; this is purely the
/// scheme-aware front door.
#[Command(default)]
pub struct Open {
    pub uri: String,
}

#[on_command(Open)]
fn on_open(trigger: On<Open>, mut commands: Commands) {
    let uri = trigger.event().uri.clone();
    if uri.is_empty() {
        bevy::log::warn!("[Open] empty uri");
        return;
    }

    // Scheme detection: if the URI contains `://` we route by scheme;
    // otherwise we look at content shape. A bare dot-separated string
    // with no path separators is treated as a qualified Modelica name
    // so `open("Modelica.Blocks.Examples.PID_Controller")` works.
    if uri.contains("://") {
        // bundled:// and mem:// are handled inside on_open_file's
        // scheme dispatcher; route everything else through OpenFile too
        // (it will fail with a warn for unknown schemes, which is the
        // right "tell the user" behaviour).
        commands.trigger(OpenFile { path: uri });
        return;
    }

    let looks_like_qualified_name = uri.contains('.')
        && !uri.contains('/')
        && !uri.contains('\\');
    if looks_like_qualified_name {
        commands.trigger(OpenExample { qualified: uri });
        return;
    }

    // Anything else: treat as a filesystem path.
    commands.trigger(OpenFile { path: uri });
}

/// Push a runtime input value into a compiled model's stepper.
///
/// The simulation worker reads `ModelicaModel.inputs` at every step
/// (cf. `spawn_modelica_requests`), so writing here propagates to the
/// running sim on the next tick — no recompile, no worker channel
/// extension, no squashing logic to add. The squashing the worker
/// already does on its `UpdateParameters` channel is the same code
/// path: last-writer-wins per name.
///
/// Validation: the input must be a declared input on the compiled
/// model. We check `model.inputs.contains_key(name)` rather than
/// re-parsing the AST because the compile path already filtered down
/// to the runtime-relevant subset.
///
/// See spec 033 P2 for the design rationale.
#[Command(default)]
pub struct SetModelInput {
    /// Document id whose linked entity holds the running model.
    pub doc: DocumentId,
    /// Input name to set. Must already exist on the model — this
    /// command does not introduce new inputs.
    pub name: String,
    /// New value. The API does not clamp to declared bounds; bounds
    /// enforcement is the agent's responsibility (per spec 033 FR-003
    /// out-of-scope item).
    pub value: f64,
}

#[on_command(SetModelInput)]
fn on_set_model_input(trigger: On<SetModelInput>, mut commands: Commands) {
    let doc_raw = trigger.event().doc;
    let name = trigger.event().name.clone();
    let value = trigger.event().value;
    commands.queue(move |world: &mut World| {
        match apply_set_model_input(world, doc_raw, &name, value) {
            Ok(_) => {}
            Err(e) => {
                bevy::log::warn!("[SetModelInput] {}", e.message());
            }
        }
    });
}

/// Outcome of [`apply_set_model_input`]. Carries enough context for an
/// API caller (`SetModelInputProvider`) to format a structured error
/// JSON; the in-process observer just stringifies and warn-logs.
#[derive(Debug, Clone)]
pub enum SetModelInputError {
    NoActiveDocument,
    NoLinkedEntity { doc: u64 },
    EntityMissingModel { doc: u64 },
    UnknownInput {
        doc: u64,
        name: String,
        model_name: String,
        known_inputs: Vec<String>,
    },
}

impl SetModelInputError {
    pub fn message(&self) -> String {
        match self {
            Self::NoActiveDocument => "no active document (pass `doc` explicitly)".into(),
            Self::NoLinkedEntity { doc } => format!(
                "doc {doc} has no linked entity — compile the model before setting inputs"
            ),
            Self::EntityMissingModel { doc } => format!(
                "doc {doc}'s linked entity has no `ModelicaModel` component"
            ),
            Self::UnknownInput { name, model_name, known_inputs, .. } => format!(
                "input `{name}` not declared on `{model_name}`. \
                 Known inputs: [{}]",
                known_inputs.join(", ")
            ),
        }
    }
}

/// Shared mutation: write a runtime input value into the simulation
/// worker's input slot for `doc`. Used by both the [`SetModelInput`]
/// Reflect-event observer and the API surface's
/// `SetModelInputProvider` (in `crate::api_queries`) so the two paths
/// can never drift.
///
/// `doc_raw.is_unassigned()` means "active document" — same convention as
/// [`SetModelInput`]'s wire form.
pub fn apply_set_model_input(
    world: &mut World,
    doc_raw: DocumentId,
    name: &str,
    value: f64,
) -> Result<DocumentId, SetModelInputError> {
    let doc = if doc_raw.is_unassigned() {
        resolve_active_doc(world).ok_or(SetModelInputError::NoActiveDocument)?
    } else {
        doc_raw
    };
    let registry = world.resource::<crate::ui::state::ModelicaDocumentRegistry>();
    let entities = registry.entities_linked_to(doc);
    drop(registry);
    let Some(entity) = entities.first().copied() else {
        return Err(SetModelInputError::NoLinkedEntity { doc: doc.raw() });
    };
    let Some(mut model) = world.get_mut::<crate::ModelicaModel>(entity) else {
        return Err(SetModelInputError::EntityMissingModel { doc: doc.raw() });
    };
    if !model.inputs.contains_key(name) {
        let known: Vec<String> = model.inputs.keys().cloned().collect();
        return Err(SetModelInputError::UnknownInput {
            doc: doc.raw(),
            name: name.to_string(),
            model_name: model.model_name.clone(),
            known_inputs: known,
        });
    }
    model.inputs.insert(name.to_string(), value);
    bevy::log::info!("[SetModelInput] doc={} {}={}", doc.raw(), name, value);
    Ok(doc)
}

#[on_command(GetFile)]
fn on_get_file(trigger: On<GetFile>) {
    let path = trigger.event().path.clone();
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            bevy::log::info!(
                "[GetFile] {} ({} bytes) -- BEGIN --\n{}\n-- END --",
                path,
                content.len(),
                content,
            );
        }
        Err(e) => {
            bevy::log::warn!("[GetFile] {} read failed: {}", path, e);
        }
    }
}

#[on_command(Exit)]
fn on_exit(_trigger: On<Exit>, mut commands: Commands) {
    bevy::log::info!("[Exit] AppExit triggered via API");
    commands.queue(|world: &mut World| {
        if let Some(mut messages) =
            world.get_resource_mut::<bevy::ecs::message::Messages<bevy::app::AppExit>>()
        {
            messages.write(bevy::app::AppExit::Success);
        }
    });
}

#[on_command(PanCanvas)]
fn on_pan_canvas(trigger: On<PanCanvas>, mut commands: Commands) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        let doc = if ev.doc.is_unassigned() {
            resolve_active_doc(world)
        } else {
            Some(ev.doc)
        };
        use crate::ui::panels::canvas_diagram::CanvasDiagramState;
        let Some(mut state) = world.get_resource_mut::<CanvasDiagramState>() else {
            return;
        };
        let docstate = state.get_mut(doc);
        let z = docstate.canvas.viewport.zoom;
        docstate.canvas.viewport.set_target(lunco_canvas::Pos::new(ev.x, ev.y), z);
    });
}

#[on_command(MoveComponent)]
fn on_move_component(trigger: On<MoveComponent>, mut commands: Commands) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        use crate::document::ModelicaOp;
        use crate::pretty::Placement;
        let active_doc = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document);
        let Some(doc_id) = active_doc else {
            bevy::log::warn!("[MoveComponent] no active document");
            return;
        };
        let class = if ev.class.is_empty() {
            // Mirror canvas_diagram::resolve_doc_context: drilled
            // scope first, then the registry's detected name.
            crate::ui::panels::model_view::drilled_class_for_doc(world, doc_id)
                .or_else(|| crate::ui::state::detected_name_for(world, doc_id))
                .unwrap_or_default()
        } else {
            ev.class.clone()
        };
        if class.is_empty() {
            bevy::log::warn!("[MoveComponent] could not resolve target class for doc");
            return;
        }
        // Use specified extent if provided, otherwise preserve the
        // node's current rect from the canvas scene (same logic as
        // the mouse-drag path).
        let (width, height) = if ev.width > 0.0 && ev.height > 0.0 {
            (ev.width, ev.height)
        } else {
            use crate::ui::panels::canvas_diagram::CanvasDiagramState;
            world
                .get_resource::<CanvasDiagramState>()
                .and_then(|state| {
                    let docstate = state.get(Some(doc_id));
                    docstate.canvas.scene.nodes().find_map(|(_id, n)| {
                        if n.origin.as_deref() == Some(ev.name.as_str()) {
                            Some((n.rect.width().max(1.0), n.rect.height().max(1.0)))
                        } else {
                            None
                        }
                    })
                })
                .unwrap_or((20.0, 20.0))
        };
        let op = ModelicaOp::SetPlacement {
            class: class.clone(),
            name: ev.name.clone(),
            placement: Placement {
                x: ev.x,
                y: ev.y,
                width,
                height,
            },
        };
        crate::ui::panels::canvas_diagram::apply_ops_public(world, doc_id, vec![op]);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_preserves_line_comments_above_class() {
        let src = "// hello world\n// more info\nmodel Foo\n  parameter Real x = 1;\nend Foo;\n";
        let got = extract_class_source(src, "Foo").expect("should extract");
        assert!(got.contains("// hello world"), "got: {got}");
        assert!(got.contains("// more info"), "got: {got}");
        assert!(got.contains("end Foo;"));
    }

    #[test]
    fn extract_preserves_block_comments_above_class() {
        let src = "/* preamble\n   note */\nmodel Foo\nend Foo;\n";
        let got = extract_class_source(src, "Foo").expect("should extract");
        assert!(got.contains("/* preamble"), "got: {got}");
        assert!(got.contains("note */"), "got: {got}");
    }

    #[test]
    fn extract_stops_at_unrelated_content_above() {
        // `within` line is NOT a comment — rewind must stop before it.
        let src = "within Foo;\n// my comment\nmodel Bar\nend Bar;\n";
        let got = extract_class_source(src, "Bar").expect("should extract");
        assert!(!got.contains("within"), "within leaked: {got}");
        assert!(got.contains("// my comment"), "comment dropped: {got}");
    }
}
