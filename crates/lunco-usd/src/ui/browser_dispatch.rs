//! Routes [`lunco_workbench::BrowserAction::OpenFile`] events with USD
//! extensions (`.usda`, `.usdc`) into the [`UsdDocumentRegistry`], so a
//! click on a `.usda` row in the Twin browser opens the file in the
//! shared USD viewport — same shape as Modelica's
//! `browser_dispatch::drain_browser_actions`, just gated on a different
//! filetype.
//!
//! ## File partitioning
//!
//! [`BrowserActions::take_where`] only removes the actions whose path
//! has a `.usda` / `.usdc` extension, leaving Modelica's `.mo` opens
//! for the Modelica drain to handle in the same frame. Two crates,
//! one shared outbox, no ordering coupling.
//!
//! ## Async I/O
//!
//! Per `AGENTS.md` §7.2 we must not block the UI thread on filesystem
//! reads. The file is loaded on [`AsyncComputeTaskPool`] and the
//! resulting source is fed to the registry on a later frame via
//! [`drain_pending_usd_file_loads`].

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};
use lunco_core::on_command;
use lunco_doc::DocumentOrigin;
use lunco_workbench::{BrowserAction, BrowserActions, WorkspaceResource};
use lunco_workbench::file_ops::OpenFile;

use crate::registry::UsdDocumentRegistry;

/// Lower-cased extensions this dispatch recognises as USD files.
/// `.usdc` (binary) is included so users get a *parser failure*
/// message instead of having the click silently misrouted to another
/// domain — the openusd 0.2.0 text reader will fail on binary input
/// and [`crate::ui::viewport`] surfaces the warning.
const USD_EXTENSIONS: &[&str] = &["usda", "usdc"];

fn is_usd_open_file(action: &BrowserAction) -> bool {
    match action {
        BrowserAction::OpenFile { relative_path } => relative_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| {
                let lower = ext.to_ascii_lowercase();
                USD_EXTENSIONS.iter().any(|e| *e == lower)
            })
            .unwrap_or(false),
        _ => false,
    }
}

/// Pending file-read kicked off by [`drain_browser_actions_for_usd`].
/// Polled by [`drain_pending_usd_file_loads`] each frame until it
/// completes; the resulting source is allocated as a USD document and
/// the [`crate::ui::viewport::UsdViewportPlugin`] picks it up via the
/// standard `DocumentOpened` lifecycle observer.
struct PendingUsdLoad {
    path: PathBuf,
    task: Task<Result<String, String>>,
}

#[derive(Resource, Default)]
pub(crate) struct PendingUsdLoads {
    tasks: Vec<PendingUsdLoad>,
}

/// Drain Twin-browser `OpenFile` actions whose path looks like USD and
/// spawn an async filesystem read for each. Idempotent re-open is
/// handled lazily: if the same path is already in the registry we
/// surface it in the viewport instead of re-allocating.
pub fn drain_browser_actions_for_usd(world: &mut World) {
    let actions: Vec<BrowserAction> = {
        let mut outbox = world.resource_mut::<BrowserActions>();
        outbox.take_where(is_usd_open_file)
    };
    if actions.is_empty() {
        return;
    }

    let twin_root = {
        let ws = world.resource::<WorkspaceResource>();
        ws.active_twin
            .and_then(|id| ws.twin(id))
            .map(|t| t.root.clone())
    };
    let Some(root) = twin_root else {
        for a in &actions {
            bevy::log::warn!(
                "BrowserAction::OpenFile (USD) fired with no active Twin: {:?}",
                a
            );
        }
        return;
    };

    for action in actions {
        let BrowserAction::OpenFile { relative_path } = action else {
            continue;
        };
        let abs = root.join(&relative_path);
        spawn_usd_load(world, abs);
    }
}

/// Shared helper: spawn the async file-read for `abs_path` and queue
/// the result in [`PendingUsdLoads`]. Callers should have already
/// established that the path looks like a USD file.
fn spawn_usd_load(world: &mut World, abs_path: PathBuf) {
    let pool = AsyncComputeTaskPool::get();
    let path_for_task = abs_path.clone();
    let task = pool.spawn(async move {
        std::fs::read_to_string(&path_for_task)
            .map_err(|e| format!("failed to read {}: {e}", path_for_task.display()))
    });
    world
        .resource_mut::<PendingUsdLoads>()
        .tasks
        .push(PendingUsdLoad { path: abs_path, task });
}

/// Observer for the workbench's typed [`OpenFile`] command. Picks up
/// `.usda` / `.usdc` paths so HTTP API / MCP / `Open` URI dispatch all
/// route into the same async-load pipeline the Twin-browser uses.
/// Modelica's `on_open_file` ignores non-`.mo` paths, so the two
/// observers coexist on the same command without stepping on each
/// other.
#[on_command(OpenFile)]
pub fn on_open_file_for_usd(trigger: On<OpenFile>, mut commands: Commands) {
    let path = trigger.event().path.clone();
    bevy::log::info!("[UsdOpenFile] observer fired for path={}", path);
    commands.queue(move |world: &mut World| {
        if path.is_empty() || path.starts_with("mem://") || path.starts_with("bundled://") {
            return;
        }
        let stripped = path.strip_prefix("file://").unwrap_or(&path);
        let path_buf = PathBuf::from(stripped);
        let is_usd = path_buf
            .extension()
            .and_then(|e| e.to_str())
            .map(|ext| {
                let lower = ext.to_ascii_lowercase();
                USD_EXTENSIONS.iter().any(|e| *e == lower)
            })
            .unwrap_or(false);
        if !is_usd {
            return;
        }
        spawn_usd_load(world, path_buf);
    });
}

/// Poll outstanding [`PendingUsdLoads`] and finish the open once each
/// file's bytes are in hand. Skips and warns on read errors —
/// continuing leaves no half-loaded document behind.
pub fn drain_pending_usd_file_loads(world: &mut World) {
    if world.resource::<PendingUsdLoads>().tasks.is_empty() {
        return;
    }

    // Pull the whole vec out so we can mutate the registry while
    // iterating. We re-insert anything still pending afterwards.
    let mut taken = std::mem::take(&mut world.resource_mut::<PendingUsdLoads>().tasks);
    let mut still_pending: Vec<PendingUsdLoad> = Vec::new();

    for mut load in taken.drain(..) {
        match block_on(future::poll_once(&mut load.task)) {
            None => still_pending.push(load),
            Some(Err(err)) => {
                bevy::log::warn!("[UsdBrowserDispatch] {}", err);
            }
            Some(Ok(source)) => {
                // Idempotent re-open: if this exact path already lives
                // in the registry, just surface it in the viewport.
                let existing = {
                    let reg = world.resource::<UsdDocumentRegistry>();
                    reg.ids().find(|id| {
                        reg.host(*id)
                            .map(|h| match h.document().origin() {
                                DocumentOrigin::File { path, .. } => path == &load.path,
                                _ => false,
                            })
                            .unwrap_or(false)
                    })
                };
                let _doc_id = match existing {
                    Some(id) => id,
                    None => world
                        .resource_mut::<UsdDocumentRegistry>()
                        .allocate(source, DocumentOrigin::writable_file(load.path.clone())),
                };
                // Only register the document — don't auto-open a
                // viewport tab. The user surfaces a preview explicitly
                // by clicking the stage's row in the USD browser
                // section.
            }
        }
    }

    world.resource_mut::<PendingUsdLoads>().tasks = still_pending;
}
