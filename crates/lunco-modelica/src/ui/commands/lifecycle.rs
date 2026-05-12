//! Document lifecycle commands — creation, opening, duplication, and closing.

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_doc::{DocumentId, DocumentOrigin};
use lunco_doc_bevy::{CloseDocument, DocumentSaved};
use lunco_workbench::file_ops::{NewDocument, OpenFile};
use lunco_core::{Command, on_command};
use std::sync::Arc;

use crate::document::duplicate::{
    collect_parent_imports, extract_class_spans_inline,
    rewrite_inject_in_one_pass,
};
use crate::ui::{
    CompileStates, ModelicaDocumentRegistry, WorkbenchState,
};
use crate::ui::panels::model_view::{ModelTabs, MODEL_VIEW_KIND};
use crate::ui::panels::package_browser::PackageTreeCache;

// ─── Command Structs ─────────────────────────────────────────────────────────

/// Request to create a new untitled Modelica model and open its tab.
#[Command(default)]
pub struct CreateNewScratchModel {}

/// Request to duplicate a read-only (library) model into a new
/// editable Untitled document.
#[Command(default)]
pub struct DuplicateModelFromReadOnly {
    pub source_doc: DocumentId,
}

/// API shim: duplicate the active read-only document into a fresh
/// editable workspace tab.
#[Command(default)]
pub struct DuplicateActiveDoc {
    pub doc: DocumentId,
}

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

/// Open (or focus, if already open) an MSL class as a fresh editable
/// copy.
#[Command(default)]
pub struct OpenExample {
    pub qualified: String,
}

/// Open the same document in a new tab (split / sibling view).
#[Command(default)]
pub struct OpenInNewView {
    pub doc: DocumentId,
}

/// Unified open command — dispatches on the URI scheme.
#[Command(default)]
pub struct Open {
    pub uri: String,
}

// ─── Resources ───────────────────────────────────────────────────────────────

#[derive(Resource, Default)]
pub struct CloseDialogState {
    pub pending: Vec<(DocumentId, u64)>,
    pub requested: std::collections::HashMap<(DocumentId, u64), lunco_ui::modal::ModalId>,
}

#[derive(Resource, Default)]
pub struct PendingCloseAfterSave {
    docs: std::collections::HashMap<DocumentId, Vec<u64>>,
}

impl PendingCloseAfterSave {
    pub fn queue(&mut self, doc: DocumentId, tab: u64) {
        self.docs.entry(doc).or_default().push(tab);
    }
    pub fn take(&mut self, doc: DocumentId) -> Vec<u64> {
        self.docs.remove(&doc).unwrap_or_default()
    }
}

// ─── Observers ───────────────────────────────────────────────────────────────

#[on_command(CreateNewScratchModel)]
pub fn on_create_new_scratch_model(
    _trigger: On<CreateNewScratchModel>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut cache: ResMut<PackageTreeCache>,
    mut model_tabs: ResMut<ModelTabs>,
    mut workbench: ResMut<WorkbenchState>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
    mut commands: Commands,
) {
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

    let source = format!("model {name}\nend {name};\n");
    let mem_id = format!("mem://{name}");
    let doc_id = registry.allocate_with_origin(
        source.clone(),
        DocumentOrigin::untitled(name.clone()),
    );

    cache.in_memory_models.retain(|e| e.id != mem_id);
    cache
        .in_memory_models
        .push(crate::ui::panels::package_browser::InMemoryEntry {
            display_name: name,
            id: mem_id,
            doc: doc_id,
        });

    let source_arc: Arc<str> = source.into();
    workbench.editor_buffer = source_arc.to_string();
    workbench.diagram_dirty = true;

    workspace.active_document = Some(doc_id);

    let tab_id = model_tabs.ensure_for(doc_id, None);
    commands.trigger(lunco_workbench::OpenTab {
        kind: MODEL_VIEW_KIND,
        instance: tab_id,
    });
}

#[on_command(DuplicateModelFromReadOnly)]
pub fn on_duplicate_model_from_read_only(
    trigger: On<DuplicateModelFromReadOnly>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut cache: ResMut<PackageTreeCache>,
    mut model_tabs: ResMut<ModelTabs>,
    mut duplicate_loads: ResMut<crate::ui::panels::canvas_diagram::DuplicateLoads>,
    mut bus: ResMut<lunco_workbench::status_bus::StatusBus>,
    mut console: ResMut<crate::ui::panels::console::ConsoleLog>,
    mut commands: Commands,
    mut egui_q: Query<&mut bevy_egui::EguiContext>,
) {
    let source_doc = trigger.event().source_doc;

    let (source_full, origin_class_short, origin_fqn, inner_drill) = {
        let Some(host) = registry.host(source_doc) else {
            console.error("Duplicate failed: source doc not found in registry");
            return;
        };
        let doc = host.document();
        let fqn = model_tabs.drilled_class_for_doc(source_doc);
        let ast_opt = doc.strict_ast();
        let top_short = ast_opt
            .as_ref()
            .and_then(|ast| ast.classes.iter().next().map(|(n, _)| n.clone()))
            .or_else(|| {
                fqn.as_ref()
                    .and_then(|q| q.split('.').next().map(String::from))
            })
            .unwrap_or_else(|| doc.origin().display_name());
        
        let inner_drill: Option<String> = fqn.as_ref().and_then(|q| {
            let suffix = q.rsplit('.').next().unwrap_or("");
            (suffix != top_short).then(|| {
                let after_top = q
                    .split('.')
                    .skip_while(|seg| *seg != top_short)
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(".");
                after_top
            }).filter(|s| !s.is_empty())
        });
        (doc.source().to_string(), top_short, fqn, inner_drill)
    };

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

    let doc_id = registry.reserve_id();

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
    if let Some(tab) = model_tabs.get_mut(tab_id) {
        tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
    }
    commands.trigger(lunco_workbench::OpenTab {
        kind: MODEL_VIEW_KIND,
        instance: tab_id,
    });

    let origin_short_for_task = origin_class_short.clone();
    let name_for_task = name.clone();
    let origin_fqn_for_task = origin_fqn;
    let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
        let class_src = source_full;
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
        crate::document::ModelicaDocument::with_origin(
            doc_id,
            copy_src,
            DocumentOrigin::untitled(name_for_task),
        )
    });

    let busy = bus.begin(
        lunco_workbench::status_bus::BusyScope::Document(doc_id.0),
        "duplicate",
        format!("Duplicating {origin_class_short} → {name}"),
    );
    duplicate_loads.insert(
        doc_id,
        crate::ui::panels::canvas_diagram::DuplicateBinding {
            display_name: name.clone(),
            origin_short: origin_class_short.clone(),
            inner_drill: inner_drill,
            started: web_time::Instant::now(),
            task,
            _busy: busy,
        },
    );
    console.info(format!(
        "📄 Duplicating `{origin_class_short}` → `{name}` (building…)"
    ));
    for mut ctx in egui_q.iter_mut() {
        ctx.get_mut().request_repaint();
    }
}

#[on_command(DuplicateActiveDoc)]
pub fn on_duplicate_active_doc(trigger: On<DuplicateActiveDoc>, mut commands: Commands) {
    let raw = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let doc = if raw.is_unassigned() {
            super::resolve_active_doc(world)
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

#[on_command(OpenClass)]
pub fn on_open_class(trigger: On<OpenClass>, mut commands: Commands) {
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

pub fn spawn_duplicate_class_task(world: &mut World, qualified: String, name_hint: String) {
    let origin_short = qualified
        .rsplit('.')
        .next()
        .map(str::to_string)
        .unwrap_or_else(|| qualified.clone());

    let taken: std::collections::HashSet<String> = world
        .resource::<PackageTreeCache>()
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

    let doc_id = world
        .resource_mut::<ModelicaDocumentRegistry>()
        .reserve_id();
    let mem_id = format!("mem://{name}");
    {
        let mut cache = world
            .resource_mut::<PackageTreeCache>();
        cache.in_memory_models.retain(|e| e.id != mem_id);
        cache
            .in_memory_models
            .push(crate::ui::panels::package_browser::InMemoryEntry {
                display_name: name.clone(),
                id: mem_id,
                doc: doc_id,
            });
    }
    let tab_id = {
        let mut model_tabs = world
            .resource_mut::<ModelTabs>();
        let tab_id = model_tabs.ensure_for(doc_id, None);
        if let Some(tab) = model_tabs.get_mut(tab_id) {
            tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        }
        tab_id
    };
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: MODEL_VIEW_KIND,
        instance: tab_id,
    });

    let qualified_for_task = qualified.clone();
    let origin_short_for_task = origin_short.clone();
    let name_for_task = name.clone();
    let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
        let Some(path) = crate::library_fs::resolve_class_path_indexed(&qualified_for_task) else {
            return crate::document::ModelicaDocument::with_origin(
                doc_id,
                format!("// Could not locate MSL file for {qualified_for_task}\n"),
                DocumentOrigin::untitled(name_for_task),
            );
        };
        let source_full = lunco_assets::msl::global_msl_source()
            .and_then(|s| s.read(&path))
            .and_then(|b| String::from_utf8(b).ok())
            .unwrap_or_default();
        
        let spans_opt = crate::document::duplicate::extract_class_spans_via_path(
            &path,
            &source_full,
            &origin_short_for_task,
        )
        .filter(|s| s.full_start < s.full_end && s.full_end <= source_full.len());
        let class_src = match spans_opt.as_ref() {
            Some(s) => source_full[s.full_start..s.full_end].to_string(),
            None => source_full,
        };
        let imports = collect_parent_imports(&path);
        let renamed = match spans_opt.as_ref() {
            Some(spans) => rewrite_inject_in_one_pass(
                &class_src,
                &name_for_task,
                &imports,
                spans,
            )
            .unwrap_or_else(|| class_src.clone()),
            None => {
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
        crate::document::ModelicaDocument::with_origin(
            doc_id,
            copy_src,
            DocumentOrigin::untitled(name_for_task),
        )
    });

    let busy = world
        .resource_mut::<lunco_workbench::status_bus::StatusBus>()
        .begin(
            lunco_workbench::status_bus::BusyScope::Document(doc_id.0),
            "duplicate",
            format!("Opening {qualified} → {name}"),
        );
    world
        .resource_mut::<crate::ui::panels::canvas_diagram::DuplicateLoads>()
        .insert(
            doc_id,
            crate::ui::panels::canvas_diagram::DuplicateBinding {
                display_name: name.clone(),
                origin_short: origin_short,
                inner_drill: None,
                started: web_time::Instant::now(),
                task,
                _busy: busy,
            },
        );
    world
        .resource_mut::<crate::ui::panels::console::ConsoleLog>()
        .info(format!(
            "📄 Opening class `{qualified}` → editable `{name}` (building…)"
        ));
}

#[on_command(OpenExample)]
pub fn on_open_example(
    trigger: On<OpenExample>,
    mut commands: Commands,
) {
    let qualified = trigger.event().qualified.clone();
    commands.trigger(OpenClass {
        qualified,
        action: ClassAction::Duplicate { name: String::new() },
    });
}

#[on_command(OpenInNewView)]
pub fn on_open_in_new_view(trigger: On<OpenInNewView>, mut commands: Commands) {
    let doc = trigger.event().doc;
    commands.queue(move |world: &mut World| {
        let drilled = world
            .get_resource::<ModelTabs>()
            .and_then(|t| t.drilled_class_for_doc(doc));
        let new_id = world
            .resource_mut::<ModelTabs>()
            .open_new(doc, drilled);
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: MODEL_VIEW_KIND,
            instance: new_id,
        });
    });
}

#[on_command(OpenFile)]
pub fn on_open_file(trigger: On<OpenFile>, mut commands: Commands) {
    let path = trigger.event().path.clone();
    commands.queue(move |world: &mut World| {
        if let Some(filename) = path.strip_prefix("bundled://") {
            open_bundled_in_world(world, filename);
            return;
        }
        if let Some(name) = path.strip_prefix("mem://") {
            focus_in_memory_doc(world, name);
            return;
        }

        let lower = path.to_ascii_lowercase();
        let is_modelica = std::path::Path::new(&lower)
            .extension()
            .and_then(|s| s.to_str())
            .map(|ext| ext == "mo")
            .unwrap_or(false);
        if !is_modelica {
            return;
        }

        // Read the file off the main thread. A 150 KB MSL package
        // file synchronously read on the input path is ~30 ms of
        // stutter; spawn on AsyncCompute and re-enter the World via
        // a one-shot channel drained on the Update tick.
        let path_buf = std::path::PathBuf::from(&path);
        let path_for_task = path_buf.clone();
        let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            std::fs::read_to_string(&path_for_task)
        });
        bevy::tasks::AsyncComputeTaskPool::get()
            .spawn(async move {
                let read_result = task.await;
                let _ = OPEN_FILE_RESULT_TX
                    .get_or_init(|| {
                        let (tx, rx) = std::sync::mpsc::channel::<OpenFileResult>();
                        let _ = OPEN_FILE_RESULT_RX.set(std::sync::Mutex::new(rx));
                        tx
                    })
                    .send(OpenFileResult {
                        path: path_buf,
                        read_result,
                    });
            })
            .detach();
    });
}

struct OpenFileResult {
    path: std::path::PathBuf,
    read_result: std::io::Result<String>,
}

static OPEN_FILE_RESULT_TX: std::sync::OnceLock<std::sync::mpsc::Sender<OpenFileResult>> =
    std::sync::OnceLock::new();
static OPEN_FILE_RESULT_RX: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Receiver<OpenFileResult>>> =
    std::sync::OnceLock::new();

/// Drain pending `OpenFile` reads and install them as documents.
/// Runs each tick; cheap when the queue is empty.
pub fn drain_open_file_results(world: &mut bevy::prelude::World) {
    let Some(rx_mutex) = OPEN_FILE_RESULT_RX.get() else {
        return;
    };
    let pending: Vec<OpenFileResult> = {
        let Ok(rx) = rx_mutex.lock() else {
            return;
        };
        rx.try_iter().collect()
    };
    for result in pending {
        let path = result.path;
        let source = match result.read_result {
            Ok(s) => s,
            Err(e) => {
                bevy::log::warn!("[OpenFile] {} read failed: {}", path.display(), e);
                continue;
            }
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Opened")
            .to_string();
        let mut registry =
            world.resource_mut::<ModelicaDocumentRegistry>();
        let doc_id = registry.allocate_with_origin(
            source,
            DocumentOrigin::File {
                path: path.clone(),
                writable: true,
            },
        );
        let mut tabs = world.resource_mut::<ModelTabs>();
        let tab_id = tabs.ensure_for(doc_id, None);
        if let Some(tab) = tabs.get_mut(tab_id) {
            tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        }
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: MODEL_VIEW_KIND,
            instance: tab_id,
        });
        bevy::log::info!("[OpenFile] opened `{}` as `{}`", path.display(), stem);
    }
}

pub fn open_bundled_in_world(world: &mut World, tail: &str) {
    // Bundled inner-class ids look like `<file>#<qualified>`; the
    // filename portion drives source lookup, the fragment (if any)
    // is the drill-in target for the canvas projector.
    let (filename, drill_in) = match tail.split_once('#') {
        Some((f, q)) => (f, Some(q.to_string())),
        None => (tail, None),
    };
    let Some(source) = crate::models::get_model(filename) else {
        bevy::log::warn!("[OpenFile] no bundled model named `{}`", filename);
        return;
    };
    let display_name = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename)
        .to_string();
    let mut registry =
        world.resource_mut::<ModelicaDocumentRegistry>();
    let doc_id = registry.allocate_with_origin(
        source.to_string(),
        DocumentOrigin::untitled(display_name.clone()),
    );
    let mut tabs = world.resource_mut::<ModelTabs>();
    let tab_id = tabs.ensure_for(doc_id, None);
    if let Some(tab) = tabs.get_mut(tab_id) {
        tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        if drill_in.is_some() {
            tab.drilled_class = drill_in;
        }
    }
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: MODEL_VIEW_KIND,
        instance: tab_id,
    });
    bevy::log::info!("[OpenFile] opened bundled `{}` as `{}`", filename, display_name);
}

pub fn focus_in_memory_doc(world: &mut World, name: &str) {
    let target_id = format!("mem://{}", name);
    let cache = world.resource::<PackageTreeCache>();
    let entry = cache
        .in_memory_models
        .iter()
        .find(|e| e.id == target_id)
        .map(|e| e.doc);
    let Some(doc_id) = entry else {
        bevy::log::warn!(
            "[OpenFile] no Untitled doc named `{}` (mem:// requires an existing tab)",
            name
        );
        return;
    };
    let tab_id = world
        .resource_mut::<ModelTabs>()
        .ensure_for(doc_id, None);
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: MODEL_VIEW_KIND,
        instance: tab_id,
    });
}

#[on_command(Open)]
pub fn on_open(trigger: On<Open>, mut commands: Commands) {
    let uri = trigger.event().uri.clone();
    if uri.is_empty() {
        bevy::log::warn!("[Open] empty uri");
        return;
    }

    if uri.contains("://") {
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

    commands.trigger(OpenFile { path: uri });
}

#[on_command(CloseDocument)]
pub fn on_close_document(
    trigger: On<CloseDocument>,
    mut registry: ResMut<ModelicaDocumentRegistry>,
) {
    let doc = trigger.event().doc;
    if registry.host(doc).is_none() {
        return;
    }
    registry.remove_document(doc);
}

pub fn on_document_closed_cleanup(
    trigger: On<CloseDocument>,
    mut model_tabs: ResMut<ModelTabs>,
    mut cache: ResMut<PackageTreeCache>,
    mut compile_states: ResMut<CompileStates>,
    mut workbench: ResMut<WorkbenchState>,
    mut workspace: ResMut<lunco_workbench::WorkspaceResource>,
) {
    let doc = trigger.event().doc;
    model_tabs.close(doc);
    cache.in_memory_models.retain(|e| e.doc != doc);
    compile_states.remove(doc);
    if workspace.active_document == Some(doc) {
        workspace.active_document = None;
        workbench.editor_buffer.clear();
    }
}

pub fn finish_close_after_save(
    trigger: On<DocumentSaved>,
    pending: Option<ResMut<PendingCloseAfterSave>>,
    mut commands: Commands,
) {
    let Some(mut pending) = pending else { return };
    let doc = trigger.event().doc;
    let tab_ids = pending.take(doc);
    if tab_ids.is_empty() {
        return;
    }
    commands.queue(move |world: &mut World| {
        for tab_id in tab_ids {
            world.commands().trigger(lunco_workbench::CloseTab {
                kind: MODEL_VIEW_KIND,
                instance: tab_id,
            });
            if let Some(mut tabs) = world
                .get_resource_mut::<ModelTabs>()
            {
                tabs.close_tab(tab_id);
            }
            if let Some(mut state) = world
                .get_resource_mut::<crate::ui::panels::canvas_diagram::CanvasDiagramState>()
            {
                state.drop_tab(tab_id);
            }
        }
        let last_gone = world
            .resource::<ModelTabs>()
            .count_for_doc(doc)
            == 0;
        if last_gone {
            world.commands().trigger(CloseDocument { doc });
        }
    });
}

pub fn drain_pending_tab_closes(
    mut pending: ResMut<lunco_workbench::PendingTabCloses>,
    registry: Res<ModelicaDocumentRegistry>,
    mut model_tabs: ResMut<ModelTabs>,
    mut dialogs: ResMut<CloseDialogState>,
    mut commands: Commands,
) {
    for tab in pending.drain() {
        let lunco_workbench::TabId::Instance { kind, instance } = tab else {
            continue;
        };
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
        if kind != MODEL_VIEW_KIND {
            continue;
        }
        let Some(doc) = model_tabs.get(instance).map(|s| s.doc) else {
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
        if is_dirty && !is_read_only {
            if !dialogs.pending.iter().any(|(d, t)| *d == doc && *t == instance) {
                dialogs.pending.push((doc, instance));
            }
        } else {
            commands.trigger(lunco_workbench::CloseTab { kind, instance });
            model_tabs.close_tab(instance);
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

const SAVE_LABEL: &str = "Save";
const DONT_SAVE_LABEL: &str = "Don't save";
const CANCEL_LABEL: &str = "Cancel";

pub fn render_close_dialogs(
    registry: Res<ModelicaDocumentRegistry>,
    mut dialogs: ResMut<CloseDialogState>,
    mut modals: ResMut<lunco_ui::modal::ModalQueue>,
    mut pending_save_close: Option<ResMut<PendingCloseAfterSave>>,
    mut commands: Commands,
) {
    use lunco_ui::modal::{ModalBody, ModalButton, ModalOutcome, ModalRequest};

    let pending = std::mem::take(&mut dialogs.pending);
    let mut survivors = Vec::with_capacity(pending.len());
    for (doc, originating_tab) in pending {
        let Some(host) = registry.host(doc) else {
            dialogs.requested.remove(&(doc, originating_tab));
            continue;
        };

        enum DialogAction {
            None,
            Save,
            DontSave,
            Cancel,
        }

        let key = (doc, originating_tab);
        let modal_id = match dialogs.requested.get(&key).copied() {
            Some(id) => id,
            None => {
                let document = host.document();
                let display_name = document.origin().display_name().to_string();
                let is_untitled = document.origin().is_untitled();
                let is_read_only = document.is_read_only();
                let can_save = !is_read_only;

                let body_text = if is_untitled {
                    "Your changes will be lost if you don't save them.\n\n\
                     This model has never been saved — picking Save will \
                     open a Save-As dialog to bind it to a file."
                        .to_string()
                } else if is_read_only {
                    "Your changes will be lost if you don't save them.\n\n\
                     This is a read-only library class; Save is unavailable. \
                     Use Duplicate to Workspace if you want to keep your edits."
                        .to_string()
                } else {
                    "Your changes will be lost if you don't save them.".to_string()
                };

                let mut buttons = Vec::new();
                if can_save {
                    buttons.push(ModalButton::Confirm(SAVE_LABEL.into()));
                }
                buttons.push(ModalButton::Destructive(DONT_SAVE_LABEL.into()));
                buttons.push(ModalButton::Cancel(CANCEL_LABEL.into()));

                let id = modals.request(ModalRequest {
                    title: format!("Save changes to '{display_name}'?"),
                    body: ModalBody::Custom(Arc::new(move |ui| {
                        ui.label(egui::RichText::new(&body_text).size(12.0));
                    })),
                    buttons,
                    dismiss_on_esc: true,
                });
                dialogs.requested.insert(key, id);
                survivors.push((doc, originating_tab));
                continue;
            }
        };

        let action = match modals.poll(modal_id) {
            None => DialogAction::None,
            Some(ModalOutcome::Confirmed(label)) if label == SAVE_LABEL => DialogAction::Save,
            Some(ModalOutcome::Destructive(label)) if label == DONT_SAVE_LABEL => {
                DialogAction::DontSave
            }
            Some(_) => DialogAction::Cancel,
        };

        if !matches!(action, DialogAction::None) {
            dialogs.requested.remove(&key);
        }
        match action {
            DialogAction::None => {
                survivors.push((doc, originating_tab));
            }
            DialogAction::Save => {
                if let Some(q) = pending_save_close.as_mut() {
                    q.queue(doc, originating_tab);
                }
                commands.trigger(lunco_doc_bevy::SaveDocument { doc });
            }
            DialogAction::DontSave => {
                let tab = originating_tab;
                commands.queue(move |world: &mut World| {
                    world.commands().trigger(lunco_workbench::CloseTab {
                        kind: MODEL_VIEW_KIND,
                        instance: tab,
                    });
                    if let Some(mut tabs) = world
                        .get_resource_mut::<ModelTabs>()
                    {
                        tabs.close_tab(tab);
                    }
                    if let Some(mut state) = world
                        .get_resource_mut::<crate::ui::panels::canvas_diagram::CanvasDiagramState>()
                    {
                        state.drop_tab(tab);
                    }
                    let last_gone = world
                        .resource::<ModelTabs>()
                        .count_for_doc(doc)
                        == 0;
                    if last_gone {
                        world.commands().trigger(CloseDocument { doc });
                    }
                });
            }
            DialogAction::Cancel => { }
        }
    }
    let alive: std::collections::HashSet<(DocumentId, u64)> =
        survivors.iter().copied().collect();
    let stale: Vec<((DocumentId, u64), lunco_ui::modal::ModalId)> = dialogs
        .requested
        .iter()
        .filter(|(k, _)| !alive.contains(k))
        .map(|(k, id)| (*k, *id))
        .collect();
    for (key, id) in stale {
        modals.cancel(id);
        dialogs.requested.remove(&key);
    }
    dialogs.pending = survivors;
}

#[on_command(NewDocument)]
pub fn on_new_modelica_document(trigger: On<lunco_workbench::file_ops::NewDocument>, mut commands: Commands) {
    if trigger.event().kind != "modelica" {
        return;
    }
    commands.trigger(CreateNewScratchModel {});
}

#[Command(default)]
pub struct GetFile {
    pub path: String,
}

#[on_command(GetFile)]
pub fn on_get_file(trigger: On<GetFile>) {
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

pub fn prewarm_msl_library() {
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
