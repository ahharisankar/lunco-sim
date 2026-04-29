//! Journal panel — chronological edit log for the active document.
//!
//! Bottom-dock tab next to Console / Diagnostics. A
//! [`poll_changes`] system reads each open document's `changes_since`
//! ring buffer once per Update and records new entries to a
//! [`JournalLog`] resource keyed by `DocumentId`. The panel renders
//! the active document's slice with wall-clock timestamps + a short
//! human description per entry, so users can audit every API or canvas
//! mutation without re-reading the source diff.
//!
//! Why a separate log resource (and not just iterating
//! `Document::changes_since` at render time)?
//!   * the document's ring buffer is capacity-bounded for state
//!     consumers (canvas projection, derived caches) that must not
//!     fall behind. The journal wants a longer history with wall
//!     timestamps that the document doesn't store.
//!   * polling once per frame and caching keeps render hot-path
//!     cheap and lets the journal survive document reloads.

use std::collections::{HashMap, VecDeque};

use bevy::prelude::*;
use bevy_egui::egui;
use lunco_doc::DocumentId;
use lunco_workbench::{Panel, PanelId, PanelSlot};
use web_time::Instant;

use crate::document::ModelicaChange;
use crate::ui::state::ModelicaDocumentRegistry;

/// Panel id.
pub const JOURNAL_PANEL_ID: PanelId = PanelId("modelica_journal");

/// Per-document retention. Edits are cheap; 1 000 entries covers a
/// long modelling session and still bounds memory.
const MAX_ENTRIES_PER_DOC: usize = 1_000;

#[derive(Debug, Clone)]
pub struct JournalEntry {
    pub at: Instant,
    pub generation: u64,
    pub change: ModelicaChange,
}

/// Wall-clock anchor for journal timestamps. Initialised on the first
/// recorded entry; we display `(at - SESSION_START).as_secs_f32()`
/// alongside an HH:MM:SS clock.
static SESSION_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

#[derive(Resource, Default)]
pub struct JournalLog {
    by_doc: HashMap<DocumentId, VecDeque<JournalEntry>>,
    last_seen_gen: HashMap<DocumentId, u64>,
}

impl JournalLog {
    pub fn entries_for(&self, doc: DocumentId) -> Option<&VecDeque<JournalEntry>> {
        self.by_doc.get(&doc)
    }

    fn record(&mut self, doc: DocumentId, entry: JournalEntry) {
        let buf = self.by_doc.entry(doc).or_default();
        if buf.len() >= MAX_ENTRIES_PER_DOC {
            buf.pop_front();
        }
        buf.push_back(entry);
    }
}

/// Drive system: poll each document's change ring once per Update and
/// append any new entries to the journal. Cheap when nothing changes
/// — `changes_since` returns an empty iterator and the loop exits.
pub fn poll_changes(
    registry: Res<ModelicaDocumentRegistry>,
    mut journal: ResMut<JournalLog>,
) {
    let now = Instant::now();
    for (doc_id, host) in registry.docs() {
        let last = journal.last_seen_gen.get(&doc_id).copied().unwrap_or(0);
        let doc = host.document();
        let Some(iter) = doc.changes_since(last) else {
            // History truncated past us — reset to the current
            // generation so we don't keep retrying.
            journal
                .last_seen_gen
                .insert(doc_id, doc.earliest_retained_generation());
            continue;
        };
        let mut max_gen = last;
        let mut new_entries: Vec<JournalEntry> = Vec::new();
        for (gen, change) in iter {
            new_entries.push(JournalEntry {
                at: now,
                generation: *gen,
                change: change.clone(),
            });
            if *gen > max_gen {
                max_gen = *gen;
            }
        }
        if max_gen > last {
            SESSION_START.get_or_init(|| now);
            for entry in new_entries {
                journal.record(doc_id, entry);
            }
            journal.last_seen_gen.insert(doc_id, max_gen);
        }
    }
}

fn change_summary(change: &ModelicaChange) -> (&'static str, String, egui::Color32) {
    match change {
        ModelicaChange::TextReplaced => (
            "TEXT",
            "source replaced".to_string(),
            egui::Color32::from_rgb(180, 180, 180),
        ),
        ModelicaChange::ComponentAdded { class, name } => (
            "ADD ",
            format!("{class} ← {name}"),
            egui::Color32::from_rgb(120, 200, 130),
        ),
        ModelicaChange::ComponentRemoved { class, name } => (
            "DEL ",
            format!("{class} ✗ {name}"),
            egui::Color32::from_rgb(220, 120, 120),
        ),
        ModelicaChange::ConnectionAdded { class, from, to } => (
            "WIRE",
            format!("{class}: {}.{} → {}.{}", from.component, from.port, to.component, to.port),
            egui::Color32::from_rgb(140, 180, 220),
        ),
        ModelicaChange::ConnectionRemoved { class, from, to } => (
            "UNWR",
            format!("{class}: {}.{} ⊘ {}.{}", from.component, from.port, to.component, to.port),
            egui::Color32::from_rgb(220, 160, 100),
        ),
        // Catch-all for variants added after this match (e.g.
        // PlacementChanged, ParameterChanged) — render generically
        // rather than refuse to display.
        other => (
            "EDIT",
            format!("{other:?}"),
            egui::Color32::from_rgb(180, 180, 180),
        ),
    }
}

pub struct JournalPanel;

impl Panel for JournalPanel {
    fn id(&self) -> PanelId {
        JOURNAL_PANEL_ID
    }

    fn title(&self) -> String {
        "📜 Journal".into()
    }

    fn default_slot(&self) -> PanelSlot {
        PanelSlot::Bottom
    }

    fn render(&mut self, ui: &mut egui::Ui, world: &mut World) {
        let theme = world
            .get_resource::<lunco_theme::Theme>()
            .cloned()
            .unwrap_or_else(lunco_theme::Theme::dark);
        let muted = theme.tokens.text_subdued;

        let active_doc = world
            .get_resource::<lunco_workbench::WorkspaceResource>()
            .and_then(|ws| ws.active_document);

        let entries: Vec<JournalEntry> = match (active_doc, world.get_resource::<JournalLog>()) {
            (Some(doc), Some(log)) => log
                .entries_for(doc)
                .map(|q| q.iter().cloned().collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        ui.horizontal(|ui| {
            let label = match active_doc {
                Some(_) => format!("{} entries", entries.len()),
                None => "(no active document)".to_string(),
            };
            ui.label(egui::RichText::new(label).size(10.0).color(muted));
        });
        ui.separator();

        if entries.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.label(
                    egui::RichText::new(
                        "(no edits yet — add a component, draw a connection, or paste source)",
                    )
                    .size(10.0)
                    .italics()
                    .color(muted),
                );
            });
            return;
        }

        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let session_start = SESSION_START
                    .get()
                    .copied()
                    .or_else(|| entries.first().map(|e| e.at));
                for entry in &entries {
                    let (tag, summary, color) = change_summary(&entry.change);
                    let offset = session_start
                        .and_then(|s| entry.at.checked_duration_since(s))
                        .map(|d| d.as_secs_f32())
                        .unwrap_or(0.0);
                    let ts = format!("[+{offset:>6.2}s]");
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&ts)
                                .monospace()
                                .size(10.0)
                                .color(muted),
                        );
                        ui.label(
                            egui::RichText::new(format!("g{:>4}", entry.generation))
                                .monospace()
                                .size(10.0)
                                .color(muted),
                        );
                        ui.label(
                            egui::RichText::new(tag)
                                .monospace()
                                .size(10.0)
                                .strong()
                                .color(color),
                        );
                        ui.label(
                            egui::RichText::new(&summary)
                                .monospace()
                                .size(11.0)
                                .color(theme.tokens.text),
                        );
                    });
                }
            });
    }
}
