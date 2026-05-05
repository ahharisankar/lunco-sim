//! Wasm-only autosave for Untitled / duplicated Modelica documents.
//!
//! On native the workbench has a real filesystem behind
//! `Save / Save As`, so the user persists explicitly. The browser
//! sandbox doesn't, and reloading the page silently loses everything
//! the user typed or duplicated. This plugin closes that gap with
//! `localStorage`-backed autosave:
//!
//! 1. **Save**: every `DocumentChanged` for an Untitled document
//!    writes its current source to `localStorage` under
//!    `<KEY_PREFIX><display_name>`. The save is keyed on display
//!    name (the same name `bundled_models()` uses) so the restore
//!    side can reconstruct the in-memory entry from `localStorage`
//!    alone — no extra index file.
//! 2. **Restore**: on the first frame, scan `localStorage` for
//!    entries with the prefix, allocate one Modelica document per
//!    entry, register matching `InMemoryEntry` + `WorkspaceClass`
//!    rows so they show up in the Package Browser exactly like a
//!    fresh duplicate.
//! 3. **Forget**: `DocumentClosed` removes the entry — closing a
//!    tab really discards it.
//!
//! All three paths are wasm-only via `cfg(target_arch = "wasm32")`;
//! native compiles to an empty plugin. The browser's storage quota
//! is a few MB per origin — Modelica sources are well under 100 KB
//! typically, so a session with a dozen scratch models still fits.

use bevy::prelude::*;

/// Namespace prefix for autosave entries in `localStorage`. Keeps our
/// records out of the way of any other code (extensions, future
/// `lunco-storage` backends) that touches the same `localStorage`.
#[cfg(target_arch = "wasm32")]
const KEY_PREFIX: &str = "lunco_modelica/untitled/";

/// Bevy plugin that wires the three lifecycle observers + the
/// startup restore system. Add this **after** `ModelicaPlugin` so
/// the document registry it observes is already initialised.
pub struct WasmAutosavePlugin;

impl Plugin for WasmAutosavePlugin {
    fn build(&self, app: &mut App) {
        #[cfg(target_arch = "wasm32")]
        {
            app.add_systems(bevy::prelude::Startup, restore_from_localstorage)
                .add_observer(autosave_on_changed)
                .add_observer(forget_on_closed);
        }
        let _ = app;
    }
}

#[cfg(target_arch = "wasm32")]
fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// Build the storage key for a document's display name.
#[cfg(target_arch = "wasm32")]
fn storage_key(display_name: &str) -> String {
    format!("{KEY_PREFIX}{display_name}")
}

/// Restore previously-autosaved Untitled documents at startup. One
/// allocation per entry; the existing `DocumentOpened` observers
/// (in `ui/mod.rs`) take care of registering the WorkspaceClass on
/// the side. Idempotent: re-running would no-op because we check
/// for an existing in-memory entry by display name.
#[cfg(target_arch = "wasm32")]
fn restore_from_localstorage(world: &mut World) {
    let Some(storage) = local_storage() else { return };
    let len = storage.length().unwrap_or(0);
    if len == 0 {
        return;
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    for i in 0..len {
        let Some(key) = storage.key(i).ok().flatten() else { continue };
        let Some(name) = key.strip_prefix(KEY_PREFIX) else { continue };
        let Some(source) = storage.get_item(&key).ok().flatten() else { continue };
        entries.push((name.to_string(), source));
    }
    // Sort so the restore order is deterministic across reloads —
    // localStorage iteration order is implementation-defined.
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (display_name, source) in entries {
        // Skip if a doc with this display name already exists
        // (e.g. the bundled default tab, or a re-fired Startup).
        let already = {
            let cache = world
                .get_resource::<crate::ui::panels::package_browser::PackageTreeCache>();
            cache
                .map(|c| {
                    c.in_memory_models
                        .iter()
                        .any(|e| e.display_name == display_name)
                })
                .unwrap_or(false)
        };
        if already {
            continue;
        }
        let mut registry = world.resource_mut::<crate::ui::state::ModelicaDocumentRegistry>();
        let doc_id = registry.allocate_with_origin(
            source,
            lunco_doc::DocumentOrigin::untitled(display_name.clone()),
        );
        // Register an in-memory entry so the Package Browser shows
        // the doc under "Workspace / (Untitled)". The browser's
        // existing render path picks it up; no extra UI plumbing.
        if let Some(mut cache) = world
            .get_resource_mut::<crate::ui::panels::package_browser::PackageTreeCache>()
        {
            let id = format!("mem://{display_name}");
            cache
                .in_memory_models
                .push(crate::ui::panels::package_browser::InMemoryEntry {
                    display_name,
                    id,
                    doc: doc_id,
                });
        }
    }
}

/// Persist the document's current source to `localStorage` after
/// every change. Filters to Untitled docs only — File-backed docs
/// have a real save path; library/MSL/bundled docs are read-only.
#[cfg(target_arch = "wasm32")]
fn autosave_on_changed(
    trigger: bevy::prelude::On<lunco_doc_bevy::DocumentChanged>,
    registry: bevy::prelude::Res<crate::ui::state::ModelicaDocumentRegistry>,
) {
    let Some(storage) = local_storage() else { return };
    let doc = trigger.event().doc;
    let Some(host) = registry.host(doc) else { return };
    let document = host.document();
    let origin = document.origin();
    if !origin.is_untitled() {
        return;
    }
    let key = storage_key(&origin.display_name());
    let _ = storage.set_item(&key, document.source());
}

/// Drop the autosaved entry when the user closes the tab — the
/// reload-and-find-it-back behaviour only makes sense for tabs
/// that are still part of the session.
#[cfg(target_arch = "wasm32")]
fn forget_on_closed(
    trigger: bevy::prelude::On<lunco_doc_bevy::DocumentClosed>,
    registry: bevy::prelude::Res<crate::ui::state::ModelicaDocumentRegistry>,
) {
    let Some(storage) = local_storage() else { return };
    let doc = trigger.event().doc;
    // The doc may already be gone from the registry by the time
    // `Closed` fires; `find_by_path` doesn't help here. Best-effort:
    // try to look it up; if absent, skip.
    let Some(host) = registry.host(doc) else { return };
    let origin = host.document().origin();
    if !origin.is_untitled() {
        return;
    }
    let key = storage_key(&origin.display_name());
    let _ = storage.remove_item(&key);
}
