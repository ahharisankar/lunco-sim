//! Built-in **Files** section — flat, domain-agnostic listing of every
//! file the [`lunco_twin::Twin`] indexer found.
//!
//! Always present in the Twin Browser. Defaults to *collapsed* because
//! the per-domain sections (Modelica, USD, …) are usually what the
//! user wants; Files is the escape hatch for "show me the raw layout."
//!
//! Click a row → emits [`super::BrowserAction::OpenFile`]. The host
//! app's domain dispatchers decide what "open" means per file kind
//! (Modelica → diagram tab, USD → stage tab, image → external viewer,
//! …). The Files section itself is intentionally dumb about file
//! semantics.

use bevy_egui::egui;

use super::{BrowserAction, BrowserCtx, BrowserScope, BrowserSection};

/// Map a domain kind id to its canonical file extension. Used to
/// append `.mo`, `.usda`, … to display names for unsaved drafts that
/// carry no on-disk path yet. Saved docs already include their
/// extension in `display_name`; we only synthesize when missing.
fn extension_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "Modelica" | "modelica" => Some("mo"),
        "USD" | "usd" => Some("usda"),
        _ => None,
    }
}

/// `display_name` with the appropriate extension appended when the
/// name doesn't already have one — so an Untitled Modelica draft
/// renders as `Untitled.mo`, not bare `Untitled`. Saved files keep
/// their stored name unchanged.
fn display_name_with_ext(entry: &super::UnsavedDocEntry) -> String {
    if entry.display_name.contains('.') {
        return entry.display_name.clone();
    }
    match extension_for_kind(&entry.kind) {
        Some(ext) => format!("{}.{}", entry.display_name, ext),
        None => entry.display_name.clone(),
    }
}

/// In-progress inline rename. At most one row across the section can
/// be in rename mode at a time — `target_abs` identifies which one.
/// `needs_focus` is set on entry and cleared after the first frame so
/// the `TextEdit` receives focus exactly once.
#[derive(Default)]
struct RenameInProgress {
    /// Absolute path of the entry being renamed (`twin.root.join(rel)`).
    /// Used to match against rendered rows and to scope the rename
    /// command to the correct Twin.
    target_abs: std::path::PathBuf,
    /// Absolute path of the Twin root containing the entry — captured
    /// up front so we can dispatch the rename command without
    /// re-resolving from `ctx.twins` at submit time.
    twin_root: std::path::PathBuf,
    /// Path relative to the Twin root, passed verbatim into
    /// [`RenameTwinEntry::relative_path`].
    relative_path: std::path::PathBuf,
    /// Edit buffer, initialised with the current filename (last segment
    /// only, not the full relative path).
    buffer: String,
    /// One-shot flag — focus the `TextEdit` on the first render after
    /// entering rename mode, then clear so subsequent frames don't
    /// steal focus from other widgets.
    needs_focus: bool,
}

/// The built-in Files section impl.
#[derive(Default)]
pub struct FilesSection {
    /// Inline rename state. `None` when no row is being renamed.
    rename: Option<RenameInProgress>,
}

impl BrowserSection for FilesSection {
    fn id(&self) -> &str {
        "lunco.workbench.files"
    }

    fn title(&self) -> &str {
        "Files"
    }

    fn scope(&self) -> BrowserScope {
        // The Files section IS the Files tab — domain-agnostic raw FS
        // view. The Models tab is reserved for typed-content sections
        // contributed by domain crates.
        BrowserScope::Files
    }

    fn default_open(&self) -> bool {
        // Inside the Files tab the section is the only one and should
        // be expanded by default — there's no domain section above to
        // anchor the user's eye.
        true
    }

    fn order(&self) -> u32 {
        // Renders below Modelica (100) in the unified Twin panel; the
        // standalone FilesPanel (when summoned) shows the same section.
        200
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut BrowserCtx) {
        // Render workspace documents (saved + unsaved) so the list
        // stays stable across Save — a Save shouldn't make a doc
        // disappear from the user's view of "what am I working on."
        // Unsaved drafts get a dirty dot in the theme warning colour
        // plus an italic name; saved docs render plain. Kind badges
        // are intentionally omitted — file extensions in the display
        // name carry that information for the user.
        let docs: Vec<super::UnsavedDocEntry> = ctx
            .world
            .get_resource::<super::UnsavedDocs>()
            .map(|r| r.entries.clone())
            .unwrap_or_default();
        let warning = ctx.world.resource::<lunco_theme::Theme>().tokens.warning;
        // Dirty marker is intentionally subtle — same hue as warning
        // but small and semi-transparent so it reads as a hint, not a
        // siren. The full-strength warning colour is for actual
        // problems (lints, parse errors), not unsaved drafts.
        let dirty_dot_color = egui::Color32::from_rgba_unmultiplied(
            warning.r(),
            warning.g(),
            warning.b(),
            110,
        );

        for entry in &docs {
            ui.horizontal(|ui| {
                if entry.is_unsaved {
                    ui.label(
                        egui::RichText::new("•")
                            .color(dirty_dot_color)
                            .size(8.0),
                    );
                    ui.label(
                        egui::RichText::new(display_name_with_ext(entry))
                            .italics(),
                    );
                } else {
                    ui.label(egui::RichText::new("  "));
                    ui.label(egui::RichText::new(display_name_with_ext(entry)));
                }
            });
        }

        // Collect twins out of ctx so we can re-borrow ctx.actions
        // inside each per-twin render without fighting the borrow
        // checker. Twin refs are cheap (just &Twin); the Vec is the
        // outer ctx.twins clone-of-refs.
        let twins: Vec<&lunco_twin::Twin> = ctx.twins.clone();

        if twins.is_empty() {
            if docs.is_empty() {
                ui.label(
                    egui::RichText::new("Open a Twin or folder to browse files.")
                        .weak()
                        .italics(),
                );
            }
            return;
        }

        // Divider only appears between the workspace docs and the
        // folder list — if either is empty, no line to draw.
        if !docs.is_empty() {
            ui.separator();
        }

        let row_h = ui.text_style_height(&egui::TextStyle::Body);

        // Per-frame queues. Single-click on a row queues an `OpenFile`
        // action; double-click queues a "begin rename" intent; Enter on
        // a rename TextEdit queues a `RenameTwinEntry` command. We
        // accumulate inside the nested egui closures (which can't
        // re-borrow `ctx.world` / `ctx.actions` while the closure
        // borrows `self.rename`), then dispatch in one pass after the
        // closures return. Same pattern the click buffer used.
        let mut clicks: Vec<std::path::PathBuf> = Vec::new();
        let mut begin_rename: Option<RenameInProgress> = None;
        let mut submit_rename: Option<RenameInProgress> = None;
        let mut cancel_rename = false;

        let active_rename_abs: Option<std::path::PathBuf> = self
            .rename
            .as_ref()
            .map(|r| r.target_abs.clone());

        for twin in &twins {
            let folder_name = twin
                .root
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| twin.root.to_string_lossy().to_string());
            let header_label = format!("📁  {}", folder_name);
            let hover_path = twin.root.to_string_lossy().into_owned();
            let salt = twin.root.to_string_lossy().into_owned();
            let twin_root = twin.root.clone();
            let resp = egui::CollapsingHeader::new(header_label)
                .id_salt(("twin_browser_folder", salt.clone()))
                .default_open(true)
                .show(ui, |ui| {
                    let files = twin.files();
                    if files.is_empty() {
                        ui.label(
                            egui::RichText::new("(empty)")
                                .weak()
                                .italics()
                                .small(),
                        );
                        return;
                    }
                    egui::ScrollArea::vertical()
                        .id_salt(("twin_browser_files_scroll", salt.clone()))
                        .auto_shrink([false; 2])
                        .show_rows(ui, row_h, files.len(), |ui, range| {
                            for i in range {
                                let entry = &files[i];
                                let abs = twin_root.join(&entry.relative_path);
                                let in_rename = active_rename_abs.as_deref()
                                    == Some(abs.as_path());

                                if in_rename {
                                    // Render the inline editor in place
                                    // of the label for the row that's
                                    // being renamed.
                                    let rename = self
                                        .rename
                                        .as_mut()
                                        .expect("active_rename_abs set ⇒ self.rename Some");
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(&mut rename.buffer)
                                            .desired_width(f32::INFINITY),
                                    );
                                    if rename.needs_focus {
                                        resp.request_focus();
                                        rename.needs_focus = false;
                                    }
                                    let enter = resp.lost_focus()
                                        && ui.input(|i| i.key_pressed(egui::Key::Enter));
                                    let esc = ui
                                        .input(|i| i.key_pressed(egui::Key::Escape));
                                    if enter {
                                        submit_rename = Some(RenameInProgress {
                                            target_abs: rename.target_abs.clone(),
                                            twin_root: rename.twin_root.clone(),
                                            relative_path: rename.relative_path.clone(),
                                            buffer: rename.buffer.clone(),
                                            needs_focus: false,
                                        });
                                    } else if esc
                                        || (resp.lost_focus() && !enter)
                                    {
                                        cancel_rename = true;
                                    }
                                } else {
                                    let label =
                                        entry.relative_path.display().to_string();
                                    let r = ui.selectable_label(false, label);
                                    if r.double_clicked() {
                                        let leaf = entry
                                            .relative_path
                                            .file_name()
                                            .map(|s| s.to_string_lossy().to_string())
                                            .unwrap_or_default();
                                        begin_rename = Some(RenameInProgress {
                                            target_abs: abs.clone(),
                                            twin_root: twin_root.clone(),
                                            relative_path: entry.relative_path.clone(),
                                            buffer: leaf,
                                            needs_focus: true,
                                        });
                                    } else if r.clicked() {
                                        clicks.push(entry.relative_path.clone());
                                    }
                                }
                            }
                        });
                });
            resp.header_response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(hover_path);
        }

        // Dispatch queued intents now that the egui closures have
        // released their borrows on `self` and `ctx`.
        for relative_path in clicks {
            ctx.actions.push(BrowserAction::OpenFile { relative_path });
        }
        if let Some(intent) = begin_rename {
            self.rename = Some(intent);
        }
        if let Some(req) = submit_rename {
            self.rename = None;
            // Skip the round-trip if the user didn't actually change
            // anything — saves a no-op on-disk rename + Twin reload.
            let old_leaf = req
                .relative_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let new_name = req.buffer.trim().to_string();
            if !new_name.is_empty() && new_name != old_leaf {
                ctx.world
                    .commands()
                    .trigger(super::super::file_ops::RenameTwinEntry {
                        twin_root: req.twin_root.to_string_lossy().into_owned(),
                        relative_path: req
                            .relative_path
                            .to_string_lossy()
                            .into_owned(),
                        new_name,
                    });
            }
        }
        if cancel_rename {
            self.rename = None;
        }
    }
}
