//! Drill-in and duplicate document load drivers.
//!
//! Two parallel pipelines: drill-in opens MSL classes read-only,
//! duplicate creates an editable Untitled copy. Both reserve a doc
//! id eagerly, spawn an off-thread loader on
//! `AsyncComputeTaskPool`, and install the prebuilt
//! [`crate::document::ModelicaDocument`] via
//! [`crate::ui::state::ModelicaDocumentRegistry::install_prebuilt`]
//! when the load completes. Per-frame drivers below poll the
//! pending tasks.

use bevy::prelude::*;
use crate::ui::state::ModelicaDocumentRegistry;

/// Tab-to-class binding for drill-in tabs whose document hasn't
/// been installed in the registry yet. Keyed by the reserved
/// DocumentId, valued by the qualified class name the tab is
/// waiting on.
///
/// The heavy work (file read + rumoca parse) lives in
/// [`crate::class_cache::ClassCache`]; this resource only tracks
/// which tabs care about which class. When the cache resolves,
/// [`drive_drill_in_loads`] builds a `ModelicaDocument` from the
/// cached AST + source (no second parse) and installs it into the
/// registry, clearing the binding.
///
/// The name `DrillInLoads` is preserved for minimal churn; the
/// resource is effectively "tabs waiting on a class cache entry".
#[derive(bevy::prelude::Resource, Default)]
pub struct DrillInLoads {
    pending: std::collections::HashMap<lunco_doc::DocumentId, DrillInBinding>,
}

/// Persistent `DocumentId → qualified class name` map for tabs
/// opened via drill-in. Lives for the tab's lifetime (cleared by
/// [`cleanup_removed_documents`]), so downstream systems — canvas
/// projection, especially — can ask "what class was this tab
/// drilled into?" after install has already cleared the transient
/// [`DrillInLoads`] entry.
///
/// Without this, projection for a drill-in tab can't scope to the
/// specific class: the installed `ModelicaDocument.canonical_path`
/// is the `.mo` file, which for multi-class package files doesn't
/// tell us which of the dozen classes inside the user meant.
// Drilled scope now lives on `ModelTabState.drilled_class`; readers
// go through `crate::ui::panels::model_view::drilled_class_for_doc`
// (or `ModelTabs::drilled_class_for_doc` directly when a Res<>
// borrow is in scope). Writers update the tab via `ensure_for(doc,
// Some(qualified))` or by mutating `tab.drilled_class` directly.

pub struct DrillInBinding {
    pub qualified: String,
    /// When the tab was opened. Used to show elapsed-seconds in the
    /// loading overlay so the user sees work is happening even when
    /// rumoca takes tens of seconds on large package files.
    pub started: web_time::Instant,
    /// Off-thread document load. Built via
    /// [`crate::document::ModelicaDocument::load_msl_file`] which
    /// hits rumoca's content-hash artifact cache, so a class whose
    /// containing file the engine session has already parsed
    /// installs in milliseconds. Driven by [`drive_drill_in_loads`].
    pub task: bevy::tasks::Task<Result<crate::document::ModelicaDocument, String>>,
}

/// Tab-to-task binding for duplicate-to-workspace operations whose
/// bg parse hasn't finished yet. The parse goes off the UI thread
/// because a naïve `allocate_with_origin` on a multi-KB source
/// re-runs rumoca synchronously — locked the workbench for seconds
/// in debug builds, which users (correctly) called a bug:
/// *"no operations like that must be in UI thread"*.
///
/// Same shape as [`DrillInLoads`]: the bg task returns a fully-built
/// [`crate::document::ModelicaDocument`], the driver system installs it into the
/// registry via `install_prebuilt`. Cleared on install and on
/// document removal.
#[derive(bevy::prelude::Resource, Default)]
pub struct DuplicateLoads {
    pending: std::collections::HashMap<
        lunco_doc::DocumentId,
        DuplicateBinding,
    >,
}

pub struct DuplicateBinding {
    pub display_name: String,
    pub origin_short: String,
    /// Path *within* the duplicated top class the user was drilled
    /// into when they hit Duplicate. e.g. duplicating an
    /// `AnnotatedRocketStage` package while focused on its inner
    /// `RocketStage` model lands here as `Some("RocketStage")` and
    /// the install hook seeds `DrilledInClassNames` with
    /// `<top_copy>.<inner_drill>` so the new tab opens on that
    /// same inner class. `None` when the user was on the top
    /// class itself.
    pub inner_drill: Option<String>,
    pub started: web_time::Instant,
    pub task: bevy::tasks::Task<crate::document::ModelicaDocument>,
}

impl DuplicateLoads {
    pub fn is_loading(&self, doc: lunco_doc::DocumentId) -> bool {
        self.pending.contains_key(&doc)
    }
    pub fn detail(&self, doc: lunco_doc::DocumentId) -> Option<&str> {
        self.pending.get(&doc).map(|b| b.display_name.as_str())
    }
    pub fn progress(&self, doc: lunco_doc::DocumentId) -> Option<(&str, f32)> {
        self.pending
            .get(&doc)
            .map(|b| (b.display_name.as_str(), b.started.elapsed().as_secs_f32()))
    }
    pub fn insert(
        &mut self,
        doc: lunco_doc::DocumentId,
        binding: DuplicateBinding,
    ) {
        self.pending.insert(doc, binding);
    }
}

impl DrillInLoads {
    pub fn is_loading(&self, doc: lunco_doc::DocumentId) -> bool {
        self.pending.contains_key(&doc)
    }
    pub fn detail(&self, doc: lunco_doc::DocumentId) -> Option<&str> {
        self.pending.get(&doc).map(|b| b.qualified.as_str())
    }
    /// `(qualified, seconds elapsed since tab opened)` for the
    /// loading overlay. Returns `None` if nothing is loading for
    /// this doc.
    pub fn progress(&self, doc: lunco_doc::DocumentId) -> Option<(&str, f32)> {
        self.pending
            .get(&doc)
            .map(|b| (b.qualified.as_str(), b.started.elapsed().as_secs_f32()))
    }
}

/// Bevy system: for each pending drill-in binding, check whether
/// its class has landed in [`ClassCache`]. If yes, build a
/// `ModelicaDocument` from the cached parts (no re-parse) and
/// install it in the registry.
/// Bevy system: poll pending duplicate bg tasks; `install_prebuilt`
/// the fully-built document into the registry when ready. Same
/// shape as [`drive_drill_in_loads`] but for the `Duplicate to
/// Workspace` flow.
pub fn drive_duplicate_loads(
    mut loads: bevy::prelude::ResMut<DuplicateLoads>,
    mut registry: bevy::prelude::ResMut<ModelicaDocumentRegistry>,
    mut probe: Option<bevy::prelude::ResMut<crate::FrameTimeProbe>>,
    mut egui_q: bevy::prelude::Query<&mut bevy_egui::EguiContext>,
    mut tabs: bevy::prelude::ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut commands: bevy::prelude::Commands,
) {
    use bevy::prelude::*;
    // While any duplicate is in-flight, ping egui every tick so the
    // canvas keeps repainting and the loading overlay actually
    // animates. Without this the canvas paints once at tab-open then
    // sleeps until something else requests a repaint — the overlay
    // is unreachable for the entire bg-parse window and the user sees
    // a blank canvas (verified via [Overlay] trace: no entries between
    // "ModelView rendering tab" and "duplicate: installed").
    if !loads.pending.is_empty() {
        for mut ctx in egui_q.iter_mut() {
            ctx.get_mut().request_repaint();
        }
    }
    let doc_ids: Vec<lunco_doc::DocumentId> = loads.pending.keys().copied().collect();
    let mut had_install = false;
    for doc_id in doc_ids {
        let Some(binding) = loads.pending.get_mut(&doc_id) else {
            continue;
        };
        let t_poll = web_time::Instant::now();
        let Some(doc) = futures_lite::future::block_on(
            futures_lite::future::poll_once(&mut binding.task),
        ) else {
            continue;
        };
        let poll_ms = t_poll.elapsed().as_secs_f64() * 1000.0;
        let dup_display_name = binding.display_name.clone();
        let origin_short = binding.origin_short.clone();
        let inner_drill = binding.inner_drill.clone();
        loads.pending.remove(&doc_id);
        let t_install = web_time::Instant::now();
        registry.install_prebuilt(doc_id, doc);
        let install_ms = t_install.elapsed().as_secs_f64() * 1000.0;
        info!(
            "[CanvasDiagram] duplicate: installed `{}` (from `{}`) — poll={poll_ms:.1}ms install={install_ms:.1}ms",
            dup_display_name, origin_short,
        );
        had_install = true;
        // Seed the drill-in target so the canvas projects the inner
        // model, not the package's empty top-level. Duplicating a
        // `package Foo { model Bar ... }` lands as
        // `package FooCopy { model Bar ... }`; `DrilledInClassNames`
        // points at `FooCopy.Bar` so the projection scopes the builder
        // to that class. Without this the user sees the empty-overlay
        // placeholder card and has to click into the package tree
        // manually.
        if let Some(host) = registry.host(doc_id) {
            // Read first non-package class from the per-doc Index;
            // sees optimistic patches and avoids walking the AST.
            let index = host.document().index();
            let qualified = match inner_drill.as_deref() {
                Some(rest) => Some(format!("{dup_display_name}.{rest}")),
                None => index
                    .classes
                    .values()
                    .find(|c| !matches!(c.kind, crate::index::ClassKind::Package))
                    .map(|c| c.name.clone()),
            };
            // Replace the `(doc, None)` placeholder with a fresh tab
            // bound to `(doc, Some(qualified))`. TabId bindings are
            // immutable; mutating drilled_class in place would collapse
            // distinct tabs into duplicate `(doc, drilled)` keys.
            if let Some(q) = qualified {
                let placeholder = tabs
                    .iter_mut_for_doc(doc_id)
                    .find(|(_, s)| s.drilled_class.is_none())
                    .map(|(id, _)| id);
                if let Some(old_id) = placeholder {
                    commands.trigger(lunco_workbench::CloseTab {
                        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                        instance: old_id,
                    });
                    tabs.close_tab(old_id);
                }
                let new_id = tabs.ensure_for(doc_id, Some(q));
                if let Some(tab) = tabs.get_mut(new_id) {
                    tab.view_mode =
                        crate::ui::panels::model_view::ModelViewMode::Canvas;
                }
                commands.trigger(lunco_workbench::OpenTab {
                    kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
                    instance: new_id,
                });
            }
        }
        // Pre-warm the MSL inheritance chain on a dedicated thread so
        // the projection finds inherited connectors. Same pattern as
        // the drill-in path. The duplicated copy carries `within
        // <origin package>;` so the within-prefixed qualified path
        // (e.g. `Modelica.Blocks.Continuous.PIDCopy`) gives the
        // scope-chain resolver enough context to walk up to
        // `Modelica.Blocks.Interfaces.SISO`.
        if let Some(host) = registry.host(doc_id) {
            // Read within-prefix + extends from the Index. Both are
            // pre-extracted during rebuild, so no AST walk per drill-in.
            let index = host.document().index();
            let within_prefix = index.within_path.clone().unwrap_or_default();
            let qpath = if within_prefix.is_empty() {
                dup_display_name.clone()
            } else {
                format!("{within_prefix}.{dup_display_name}")
            };
            // Fall back to the short name when the qualified path
            // isn't directly indexed (e.g. user-typed un-`within`'d
            // top-level classes).
            let entry = index
                .classes
                .get(&qpath)
                .or_else(|| index.classes.get(&dup_display_name));
            // Engine session caches across calls; the projection task
            // resolves inherited components on demand. No off-thread
            // prewarm needed.
            let _ = entry;
        }
    }
    if had_install {
        if let Some(p) = probe.as_deref_mut() {
            p.last_edit = Some(web_time::Instant::now());
        }
    }
}

pub fn drive_drill_in_loads(
    mut loads: bevy::prelude::ResMut<DrillInLoads>,
    mut registry: bevy::prelude::ResMut<ModelicaDocumentRegistry>,
    mut tabs: bevy::prelude::ResMut<crate::ui::panels::model_view::ModelTabs>,
    mut egui_q: bevy::prelude::Query<&mut bevy_egui::EguiContext>,
) {
    use bevy::prelude::*;
    // Keep egui awake while loads are in flight so the "Loading…"
    // overlay actually animates. Mirrors the duplicate-loads driver
    // — without this the canvas paints once and sleeps until input.
    if !loads.pending.is_empty() {
        for mut ctx in egui_q.iter_mut() {
            ctx.get_mut().request_repaint();
        }
    }
    let doc_ids: Vec<lunco_doc::DocumentId> = loads.pending.keys().copied().collect();
    for doc_id in doc_ids {
        let Some(binding) = loads.pending.get_mut(&doc_id) else {
            continue;
        };
        let Some(result) = futures_lite::future::block_on(
            futures_lite::future::poll_once(&mut binding.task),
        ) else {
            continue;
        };
        let qualified = binding.qualified.clone();
        loads.pending.remove(&doc_id);
        let doc = match result {
            Ok(doc) => doc,
            Err(msg) => {
                warn!(
                    "[CanvasDiagram] drill-in: class `{}` load failed: {}",
                    qualified, msg
                );
                // Surface the failure on every tab waiting on this
                // (doc, drilled_class). Without this the canvas overlay
                // would show "Loading resource…" forever.
                for (_id, state) in tabs.iter_mut_for_doc(doc_id) {
                    if state.drilled_class.as_deref() == Some(qualified.as_str()) {
                        state.load_error = Some(msg.clone());
                    }
                }
                continue;
            }
        };
        // Capture file path for the install log + smart-view decision
        // before moving the doc into the registry.
        let (file_path_display, has_components) = {
            let path = match doc.origin() {
                lunco_doc::DocumentOrigin::File { path, .. } => path.display().to_string(),
                _ => String::from("<no path>"),
            };
            // Smart default view for the drilled-in tab. Matches
            // OMEdit/Dymola: icon-only class or class with zero
            // instantiated components → Icon view; otherwise Canvas
            // (the user drilled FROM a canvas, expects a canvas).
            let has_components = doc.strict_ast().and_then(|ast| {
                crate::diagram::find_class_by_qualified_name(&ast, &qualified)
                    .map(|c| !c.components.is_empty())
            });
            (path, has_components)
        };
        registry.install_prebuilt(doc_id, doc);
        // by the upstream `drill_into_class` call before this
        // driver runs.
        let land_in_icon_view =
            crate::ui::loaded_classes::is_icon_only_class(&qualified)
                || has_components == Some(false);
        if land_in_icon_view {
            // Update the drilled-in tab's view mode. Multiple tabs
            // may now point at the same doc (sibling drill-ins);
            // scope by `(doc, qualified)`.
            if let Some(tab) = tabs.find_for_mut(doc_id, Some(qualified.as_str())) {
                tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Icon;
            }
        }
        info!(
            "[CanvasDiagram] drill-in: installed `{}` from `{}`",
            qualified, file_path_display,
        );
    }
}

/// Open the Modelica class with `qualified` name in a new tab.
/// The tab appears immediately with an empty document showing a
/// "Loading…" overlay; the file read happens on a background task
/// and the source is applied via `ReplaceSource` when the read
/// completes. This matches what users expect: the tab opens, a
/// spinner says "loading", content lands when it's ready.
pub fn drill_into_class(world: &mut World, qualified: &str) {
    // Try MSL paths first (resolves Modelica.* and any other MSL-rooted
    // qualified path). Fallback: scan the open document registry for a
    // doc whose AST contains the requested class — handles non-MSL
    // user-opened files (e.g. `assets/models/AnnotatedRocketStage.mo`)
    // where the qualified name lives only in a workspace document.
    let file_path = crate::library_fs::resolve_class_path_indexed(qualified)
        .or_else(|| crate::library_fs::locate_library_file(qualified));
    if let Some(file_path) = file_path {
        open_drill_in_tab(world, qualified, &file_path);
        return;
    }
    // Open-document fallback: find a host whose parsed AST resolves the
    // qualified path. Reuse its tab + just set the drill-in class.
    let target_doc: Option<lunco_doc::DocumentId> = {
        let registry = world.resource::<ModelicaDocumentRegistry>();
        registry.iter().find_map(|(doc_id, host)| {
            host.document().strict_ast().and_then(|ast| {
                crate::diagram::find_class_by_qualified_name(&ast, qualified)
                    .map(|_| doc_id)
            })
        })
    };
    if let Some(doc_id) = target_doc {
        // Allocate (or focus) a tab dedicated to this `(doc, class)`.
        // Distinct sibling classes from the same `.mo` file get their
        // own tabs — that's the whole point of keying ModelTabs by
        // TabId rather than DocumentId.
        let tab_id = {
            let mut tabs = world
                .resource_mut::<crate::ui::panels::model_view::ModelTabs>();
            // Drill-in is a deliberate navigation gesture (canvas
            // double-click), so the tab is pinned via ensure_for —
            // not the preview slot. Same-class re-drill focuses;
            // sibling drills still get their own tabs.
            let tab_id = tabs.ensure_for(doc_id, Some(qualified.to_string()));
            if let Some(tab) = tabs.get_mut(tab_id) {
                tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
            }
            tab_id
        };
        // `ensure_for(doc_id, Some(qualified))` immediately above
        // already wrote it.
        if let Some(mut workspace) =
            world.get_resource_mut::<lunco_workbench::WorkspaceResource>()
        {
            workspace.active_document = Some(doc_id);
        }
        world.commands().trigger(lunco_workbench::OpenTab {
            kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
            instance: tab_id,
        });
        bevy::log::info!(
            "[CanvasDiagram] drill-in: opened tab #{tab_id} for `{}` on existing doc",
            qualified,
        );
        return;
    }
    bevy::log::warn!(
        "[CanvasDiagram] drill-in: could not locate `{}` (no MSL match, no open doc with that class)",
        qualified
    );
}

/// Open a tab for `qualified` class backed by a **placeholder
/// document** — empty source, parses instantly. Spawns a bg task
/// that reads the file; a later Bevy system applies `ReplaceSource`
/// when the read completes.
///
/// The user sees:
///  1. Instant: a new tab titled with the class short name.
///  2. Immediately: an "Loading…" overlay on the canvas.
///  3. A moment later: the real source + diagram populates.
///
/// If a tab for the same file path is already open (from a
/// previous drill-in), we focus it instead of making a second.
fn open_drill_in_tab(
    world: &mut World,
    qualified: &str,
    file_path: &std::path::Path,
) {
    // Find or allocate the doc. Reuse an existing one only if the
    // same `(file, drilled-in class)` was opened before — keying on
    // file alone collapsed sibling MSL classes (e.g. `Integrator`
    // and `Derivative` both in `Continuous.mo`) onto one tab, so a
    // second drill silently focused the first tab instead of
    // showing the requested class.
    let model_path_id = format!("msl://{qualified}");
    let existing_doc = {
        let registry = world.resource::<ModelicaDocumentRegistry>();
        let tabs = world.resource::<crate::ui::panels::model_view::ModelTabs>();
        // A tab whose `(doc.file, drilled_class)` matches the new
        // request — re-focus it instead of allocating a duplicate.
        tabs.iter().find_map(|(_id, state)| {
            if state.drilled_class.as_deref() != Some(qualified) {
                return None;
            }
            let same_file = registry
                .host(state.doc)
                .and_then(|h| match h.document().origin() {
                    lunco_doc::DocumentOrigin::File { path, .. } => {
                        Some(path == file_path)
                    }
                    _ => None,
                })
                .unwrap_or(false);
            same_file.then_some(state.doc)
        })
    };
    let (doc_id, needs_load) = if let Some(id) = existing_doc {
        (id, false)
    } else {
        // Reserve a doc id only; the actual `ModelicaDocument`
        // (including the rumoca parse) is built on a background
        // thread and installed via `install_prebuilt` when ready.
        // Queries against the id before install return `None` —
        // panels render the "Loading resource…" overlay based on
        // `DrillInLoads::is_loading`.
        let mut registry = world.resource_mut::<ModelicaDocumentRegistry>();
        let id = registry.reserve_id();
        (id, true)
    };

    if needs_load {
        // Spawn the off-thread load. `load_msl_class` extracts only
        // the target class from the wrapper file: a 152 KB
        // `Modelica/Blocks/package.mo` becomes a ~7 KB doc holding
        // just `PID_Controller` + a `within Modelica.Blocks.Examples;`
        // prefix for scope-chain resolution. Lazy doc — no main-
        // thread parse on install; `drive_engine_sync` parses the
        // small slice off-thread. Driver: `drive_drill_in_loads`.
        let path_for_task = file_path.to_path_buf();
        let qualified_for_task = qualified.to_string();
        let task = bevy::tasks::AsyncComputeTaskPool::get().spawn(async move {
            crate::document::ModelicaDocument::load_msl_class(
                doc_id,
                &path_for_task,
                &qualified_for_task,
            )
        });
        let mut loads = world.resource_mut::<DrillInLoads>();
        loads.pending.insert(
            doc_id,
            DrillInBinding {
                qualified: qualified.to_string(),
                started: web_time::Instant::now(),
                task,
            },
        );
    }
    // call below is in the same stack frame, so no observer can
    // run between this point and the tab carrying the drilled
    // scope — the original race the eager bind protected against
    // doesn't exist when the source-of-truth IS the tab.

    let _ = model_path_id;

    // Register the tab + land the user in Canvas view (they
    // drilled FROM a canvas, so the canvas is what they expect
    // to see). Default `view_mode` is Text for newly-created
    // scratch models; drill-in is a different use case.
    let tab_id = {
        let mut model_tabs =
            world.resource_mut::<crate::ui::panels::model_view::ModelTabs>();
        let tab_id =
            model_tabs.ensure_for(doc_id, Some(qualified.to_string()));
        if let Some(tab) = model_tabs.get_mut(tab_id) {
            tab.view_mode = crate::ui::panels::model_view::ModelViewMode::Canvas;
        }
        tab_id
    };
    world.commands().trigger(lunco_workbench::OpenTab {
        kind: crate::ui::panels::model_view::MODEL_VIEW_KIND,
        instance: tab_id,
    });

    bevy::log::info!(
        "[CanvasDiagram] drill-in: opened placeholder tab for `{}` (file: `{}`) — loading in background",
        qualified,
        file_path.display()
    );
}
