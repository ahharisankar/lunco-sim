//! Modelica section of the Twin Browser.
//!
//! ## What it shows
//!
//! 1. Every Modelica document currently loaded in the
//!    [`ModelicaDocumentRegistry`] — drafts, duplicates from the
//!    Welcome examples, files opened in earlier sessions. This is the
//!    workspace's authoritative view of "what Modelica content does
//!    the user have right now."
//! 2. *(Future)* Files in the open Twin folder that aren't loaded yet
//!    — surfaced as a separate group so users can click to load.
//!
//! Each row is a Modelica class keyed by its **fully-qualified path**
//! (e.g. `"AnnotatedRocketStage.RocketStage"`). Click → emits
//! [`BrowserAction::OpenLoadedClass`] for in-memory docs, dispatched
//! into the existing drill-in machinery so the canvas tab opens
//! directly on the requested class.
//!
//! ## Single source of truth
//!
//! This panel **does not parse**. It reads
//! [`ModelicaDocument::syntax`](crate::document::ModelicaDocument::syntax)
//! — the lenient parse cache that the off-thread refresh in
//! [`crate::ui::ast_refresh`] keeps up to date — and derives the
//! class tree from it on each render. The browser sees exactly the
//! same parse the rest of the workbench sees; no panel-local cache
//! and no panel-local rumoca call.
//!
//! Building the [`ClassEntry`] tree from a `SyntaxCache` is sub-
//! millisecond on typical Modelica files (just walks the AST and
//! clones short strings), so we re-derive on every render rather
//! than maintain another cache layer.

use bevy_egui::egui;
use lunco_doc::DocumentId;
use lunco_workbench::{BrowserAction, BrowserCtx, BrowserSection};
use rumoca_session::parsing::ast::ClassDef;
use rumoca_session::parsing::ClassType;

use crate::document::SyntaxCache;

// `DrilledInClassNames` reads migrated to
// `crate::ui::panels::model_view::drilled_class_for_doc` (B.3).
use crate::ui::state::ModelicaDocumentRegistry;

/// One Modelica class entry rendered in the tree.
#[derive(Debug, Clone)]
struct ClassEntry {
    /// Short identifier (e.g. `"Engine"`).
    short_name: String,
    /// Fully-qualified path (e.g. `"AnnotatedRocketStage.Engine"`).
    qualified_path: String,
    /// Modelica class kind — drives the row's letter badge.
    kind: ClassType,
    /// Description string (the `"…"` after the class header), if present.
    description: Option<String>,
    /// Children — nested classes inside a package / model.
    children: Vec<ClassEntry>,
}

/// The Modelica Twin-Browser section. Stateless — every render
/// derives the class tree from
/// [`ModelicaDocument::syntax`](crate::document::ModelicaDocument::syntax),
/// which is kept up to date off-thread by [`crate::ui::ast_refresh`].
#[derive(Default)]
pub struct ModelicaSection;

impl BrowserSection for ModelicaSection {
    fn id(&self) -> &str {
        "lunco.modelica.classes"
    }

    fn title(&self) -> &str {
        "Modelica"
    }

    fn default_open(&self) -> bool {
        true
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut BrowserCtx<'_>) {
        // OMEdit-style flat list of loaded Modelica top-level
        // classes — system libraries (MSL, future ModelicaServices,
        // twin.toml externals), bundled examples, and one entry
        // per writable / Untitled workspace document. Each entry
        // gets its own `CollapsingHeader`; `LoadedClass`
        // implementations own their inner rendering. The list is a
        // live registry mutated by lifecycle observers as Twins and
        // documents come and go — see `loaded_classes.rs`.
        let mut entries = match ctx
            .world
            .remove_resource::<crate::ui::loaded_classes::LoadedModelicaClasses>()
        {
            Some(r) => r,
            None => {
                ui.label(
                    egui::RichText::new("(LoadedModelicaClasses resource missing)")
                        .weak()
                        .italics(),
                );
                return;
            }
        };

        if entries.entries.is_empty() {
            ui.label(
                egui::RichText::new("No Modelica classes loaded.")
                    .weak()
                    .italics(),
            );
        } else {
            for class in &mut entries.entries {
                let name = class.name(ctx);
                let label = if class.writable() {
                    name
                } else {
                    format!("🔒  {}", name)
                };
                let resp = egui::CollapsingHeader::new(label)
                    .id_salt(("loaded_modelica_class", class.id()))
                    .default_open(class.default_open())
                    .show(ui, |ui| class.render_children(ui, ctx));
                resp.header_response
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
            }
        }

        ctx.world.insert_resource(entries);
    }
}

/// Render the class tree of one writable / Untitled workspace
/// document. Called by [`crate::ui::loaded_classes::WorkspaceClass`] —
/// the outer `CollapsingHeader` row carrying this doc's name has
/// already been drawn; we just paint the children inline.
///
/// Source-of-truth read of [`ModelicaDocumentRegistry`] via the doc's
/// [`crate::index::ModelicaIndex`]. Stateless; the registry's
/// off-thread refresh + per-op optimistic patches keep the Index current.
pub(crate) fn render_workspace_doc(
    ui: &mut egui::Ui,
    ctx: &mut BrowserCtx<'_>,
    doc_id: DocumentId,
) {
    let (classes, has_parse_errors) = match ctx
        .world
        .get_resource::<ModelicaDocumentRegistry>()
        .and_then(|reg| reg.host(doc_id))
        .map(|host| classes_from_index(host.document().index()))
    {
        Some(t) => t,
        None => {
            ui.label(
                egui::RichText::new("(document not in registry)")
                    .weak()
                    .italics(),
            );
            return;
        }
    };

    let theme = ctx
        .world
        .get_resource::<lunco_theme::Theme>()
        .cloned()
        .unwrap_or_else(lunco_theme::Theme::dark);

    let active_doc: Option<DocumentId> = ctx
        .world
        .get_resource::<lunco_workbench::WorkspaceResource>()
        .and_then(|ws| ws.active_document);
    let active_qualified: Option<String> = active_doc.and_then(|d| {
        // B.3: derive from `ModelTabs` instead of `DrilledInClassNames`.
        crate::ui::panels::model_view::drilled_class_for_doc(ctx.world, d)
    });

    // Collapse the redundant wrapper when the document holds a
    // single top-level class whose short name matches the outer
    // header (e.g. duplicated `AnnotatedRocketStageCopy.mo` whose
    // sole top class is `package AnnotatedRocketStageCopy`). Without
    // this, the browser shows the same name twice — once on the
    // workspace doc row, once on the package row immediately below.
    // We promote the wrapper's children to the top so the inner
    // classes (Airframe, Engine, FluidPort, …) sit directly under
    // the doc header.
    let doc_display_name: Option<String> = ctx
        .world
        .get_resource::<ModelicaDocumentRegistry>()
        .and_then(|reg| reg.host(doc_id))
        .map(|host| host.document().origin().display_name());
    let classes: Vec<ClassEntry> = if classes.len() == 1
        && doc_display_name
            .as_deref()
            .map(|n| n == classes[0].short_name)
            .unwrap_or(false)
        && !classes[0].children.is_empty()
    {
        classes.into_iter().next().unwrap().children
    } else {
        classes
    };

    if classes.is_empty() {
        // Distinguish empty-draft from broken-file. A blank
        // "(no classes yet)" row on a file the user just broke
        // looks identical to a healthy empty draft — the user
        // thinks their classes were deleted. Label the error case
        // explicitly.
        let (text, color) = if has_parse_errors {
            (
                "⚠ parse error".to_string(),
                egui::Color32::from_rgb(220, 160, 60),
            )
        } else {
            (
                "(no classes yet)".to_string(),
                ui.visuals().weak_text_color(),
            )
        };
        ui.label(
            egui::RichText::new(text)
                .color(color)
                .small()
                .italics(),
        );
        return;
    }
    for class in &classes {
        render_class_row(
            ui,
            class,
            doc_id,
            active_doc,
            active_qualified.as_deref(),
            &theme,
            ctx,
        );
    }
}


/// Build the same class tree from the per-doc Index. Reads only the
/// [`crate::index::ClassEntry`]s (no AST walk). Used by the live
/// renderer; `classes_from_syntax` is kept for the test fixtures
/// below until those migrate.
fn classes_from_index(index: &crate::index::ModelicaIndex) -> (Vec<ClassEntry>, bool) {
    use crate::index::ClassKind;
    fn map_kind(k: ClassKind) -> ClassType {
        match k {
            ClassKind::Model => ClassType::Model,
            ClassKind::Block => ClassType::Block,
            ClassKind::Connector => ClassType::Connector,
            ClassKind::Package => ClassType::Package,
            ClassKind::Function => ClassType::Function,
            ClassKind::Class => ClassType::Class,
            ClassKind::Type => ClassType::Type,
            ClassKind::Record => ClassType::Record,
            ClassKind::ExpandableConnector => ClassType::Connector,
            ClassKind::Operator => ClassType::Operator,
            ClassKind::OperatorRecord => ClassType::Record,
        }
    }
    fn build_subtree(
        index: &crate::index::ModelicaIndex,
        qualified: &str,
    ) -> Option<ClassEntry> {
        let entry = index.classes.get(qualified)?;
        let short = entry
            .name
            .rsplit('.')
            .next()
            .unwrap_or(&entry.name)
            .to_string();
        let description = if entry.description.is_empty() {
            None
        } else {
            Some(entry.description.clone())
        };
        let mut children: Vec<ClassEntry> = entry
            .children
            .iter()
            .filter_map(|child_qual| build_subtree(index, child_qual))
            .collect();
        children.sort_by_key(|c| (browser_sort_group(c), c.short_name.to_lowercase()));
        Some(ClassEntry {
            short_name: short,
            qualified_path: entry.name.clone(),
            kind: map_kind(entry.kind),
            description,
            children,
        })
    }

    // Top-level classes: Index keys whose qualified name has no `.`
    let mut top: Vec<ClassEntry> = index
        .classes
        .keys()
        .filter(|k| !k.contains('.'))
        .filter_map(|k| build_subtree(index, k))
        .collect();
    top.sort_by_key(|c| (browser_sort_group(c), c.short_name.to_lowercase()));
    (top, index.has_errors)
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Test-only convenience: build a [`SyntaxCache`] from `source` and
/// derive the class tree + error flag through the same path as the
/// production renderer. Mirrors what `render` does, but starting
/// from raw source (production gets the cache from
/// [`ModelicaDocument`] via the off-thread refresh).
#[cfg(test)]
fn parse_classes(source: &str) -> (Vec<ClassEntry>, bool) {
    let syntax = SyntaxCache::from_source(source, 0);
    classes_from_syntax(&syntax)
}


/// Sort bucket for [`ClassEntry`]. Variant order = display order via
/// derived `Ord`, so adding a new bucket is a one-line edit.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
enum BrowserSortGroup {
    UsersGuide,
    Examples,
    SubPackage,
    LeafModel,
    LeafBlock,
    LeafConnector,
    LeafRecord,
    LeafFunction,
    LeafType,
    LeafClass,
    LeafOperator,
}

fn browser_sort_group(c: &ClassEntry) -> BrowserSortGroup {
    match c.short_name.as_str() {
        "UsersGuide" => BrowserSortGroup::UsersGuide,
        "Examples" => BrowserSortGroup::Examples,
        _ => match c.kind {
            ClassType::Package => BrowserSortGroup::SubPackage,
            ClassType::Model => BrowserSortGroup::LeafModel,
            ClassType::Block => BrowserSortGroup::LeafBlock,
            ClassType::Connector => BrowserSortGroup::LeafConnector,
            ClassType::Record => BrowserSortGroup::LeafRecord,
            ClassType::Function => BrowserSortGroup::LeafFunction,
            ClassType::Type => BrowserSortGroup::LeafType,
            ClassType::Class => BrowserSortGroup::LeafClass,
            ClassType::Operator => BrowserSortGroup::LeafOperator,
        },
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Paint one class row. Recurses into children when the row is
/// expanded. Click → [`BrowserAction::OpenLoadedClass`] keyed by the
/// owning document's id.
///
/// `active_doc`/`active_qualified` describe what the foreground tab
/// is currently editing; the matching row paints "selected" so users
/// see at a glance which class they're on.
fn render_class_row(
    ui: &mut egui::Ui,
    class: &ClassEntry,
    doc_id: DocumentId,
    active_doc: Option<DocumentId>,
    active_qualified: Option<&str>,
    theme: &lunco_theme::Theme,
    ctx: &mut BrowserCtx<'_>,
) {
    use crate::ui::theme::ModelicaThemeExt;
    let badge = type_badge(&class.kind, theme);
    let is_active = Some(doc_id) == active_doc
        && active_qualified == Some(class.qualified_path.as_str());

    if class.children.is_empty() {
        let resp = ui.horizontal(|ui| {
            paint_badge(ui, badge, theme);
            let label = if is_active {
                egui::RichText::new(&class.short_name).strong()
            } else {
                egui::RichText::new(&class.short_name)
            };
            ui.add(egui::Label::new(label).selectable(false).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
        }).inner;
        // Explicit highlight band — `selectable_label`'s default
        // selected chrome blends into the panel background under a
        // dark egui theme, leaving the user with no visual cue. We
        // paint the same translucent yellow the package-browser
        // tree's `render_node` uses so the active row matches across
        // both views (Twin sidebar Modelica section + standalone
        // Package Browser).
        if is_active {
            ui.painter().rect_filled(
                resp.rect,
                2.0,
                egui::Color32::from_rgba_unmultiplied(80, 80, 0, 40),
            );
        }
        if resp.clicked() {
            ctx.actions.push(BrowserAction::OpenLoadedClass {
                doc_id: doc_id.raw(),
                qualified_path: class.qualified_path.clone(),
            });
        }
        {
            // Hover stays lightweight — short name + qualified path
            // only. The docstring lives in the Docs view, not on
            // hover, so we don't duplicate content one click away.
            let muted = theme.text_muted();
            resp.on_hover_ui(|ui| {
                ui.strong(&class.short_name);
                ui.label(
                    egui::RichText::new(&class.qualified_path)
                        .small()
                        .color(muted),
                );
            });
        }
    } else {
        let mut header_text =
            egui::RichText::new(format!("{} {}", badge.letter, class.short_name));
        if is_active {
            header_text = header_text.strong();
        }
        let header = egui::CollapsingHeader::new(header_text)
            .id_salt(("modelica_class", &class.qualified_path))
            .default_open(true);
        let resp = header.show(ui, |ui| {
            for child in &class.children {
                render_class_row(
                    ui,
                    child,
                    doc_id,
                    active_doc,
                    active_qualified,
                    theme,
                    ctx,
                );
            }
        });
        let qualified = class.qualified_path.clone();
        let short = class.short_name.clone();
        let muted = theme.text_muted();
        let header_resp = resp.header_response.clone()
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        header_resp.on_hover_ui(move |ui| {
            ui.strong(&short);
            ui.label(
                egui::RichText::new(&qualified)
                    .small()
                    .color(muted),
            );
        });
        if resp.header_response.clicked() {
            ctx.actions.push(BrowserAction::OpenLoadedClass {
                doc_id: doc_id.raw(),
                qualified_path: class.qualified_path.clone(),
            });
        }
    }
}

/// Visual descriptor for a class-kind badge.
pub(crate) struct Badge {
    pub letter: &'static str,
    pub bg: egui::Color32,
}

pub(crate) fn type_badge(kind: &ClassType, theme: &lunco_theme::Theme) -> Badge {
    use crate::ui::theme::ModelicaThemeExt;
    let letter = match kind {
        ClassType::Model => "M",
        ClassType::Block => "B",
        ClassType::Class => "C",
        ClassType::Connector => "X",
        ClassType::Record => "R",
        ClassType::Type => "T",
        ClassType::Package => "P",
        ClassType::Function => "F",
        ClassType::Operator => "O",
    };
    Badge {
        letter,
        bg: theme.class_badge_bg(kind),
    }
}

/// Same badge mapping keyed by the lowercase `class_kind` string
/// carried on `MSLComponentDef` and `PackageNode::Model::class_kind`.
/// Lets the package-browser tree use the workspace section's exact
/// visual for MSL / Bundled rows without duplicating the colour
/// table. Unknown kinds fall through to `Class` (neutral colour).
pub(crate) fn type_badge_from_str(class_kind: &str, theme: &lunco_theme::Theme) -> Badge {
    let kind = match class_kind.to_ascii_lowercase().as_str() {
        "model" => ClassType::Model,
        "block" => ClassType::Block,
        "connector" => ClassType::Connector,
        "record" => ClassType::Record,
        "type" => ClassType::Type,
        "package" => ClassType::Package,
        "function" => ClassType::Function,
        "operator" => ClassType::Operator,
        _ => ClassType::Class,
    };
    type_badge(&kind, theme)
}

pub(crate) fn paint_badge(ui: &mut egui::Ui, badge: Badge, theme: &lunco_theme::Theme) {
    use crate::ui::theme::ModelicaThemeExt;
    ui.add(
        egui::Label::new(
            egui::RichText::new(badge.letter)
                .monospace()
                .small()
                .background_color(badge.bg)
                .color(theme.class_badge_fg()),
        )
        .selectable(false),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_top_level_models() {
        let src = r#"
model A end A;
model B "with description" end B;
"#;
        let (cs, errors) = parse_classes(src);
        assert!(!errors);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].short_name, "A");
        assert_eq!(cs[0].qualified_path, "A");
        assert!(matches!(cs[0].kind, ClassType::Model));
        assert_eq!(cs[1].description.as_deref(), Some("with description"));
    }

    #[test]
    fn parses_nested_classes_with_qualified_paths() {
        let src = r#"
package P
  model Inner end Inner;
  model Other "x" end Other;
end P;
"#;
        let (cs, errors) = parse_classes(src);
        assert!(!errors);
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].short_name, "P");
        assert!(matches!(cs[0].kind, ClassType::Package));
        assert_eq!(cs[0].children.len(), 2);
        assert_eq!(cs[0].children[0].qualified_path, "P.Inner");
        assert_eq!(cs[0].children[1].qualified_path, "P.Other");
    }

    #[test]
    fn empty_source_returns_empty() {
        let (cs, errors) = parse_classes("");
        assert!(cs.is_empty());
        assert!(!errors);
    }

    #[test]
    fn broken_sibling_class_does_not_wipe_the_others() {
        // Primary regression guard for the "classes disappear from
        // browser when file invalid" bug: a syntax error in the last
        // class must not remove the preceding healthy ones from the
        // tree. Uses rumoca's error recovery via `parse_to_syntax`.
        let src = r#"
model Good1 end Good1;
model Good2 end Good2;
model Broken
    Real x =   // missing RHS, broken on purpose
end Broken;
"#;
        let (cs, errors) = parse_classes(src);
        assert!(errors, "parse should report errors on the broken class");
        let names: Vec<&str> = cs.iter().map(|c| c.short_name.as_str()).collect();
        assert!(
            names.contains(&"Good1") && names.contains(&"Good2"),
            "healthy sibling classes must survive recovery, got {names:?}"
        );
    }

    #[test]
    fn totally_broken_file_signals_error_even_when_empty() {
        // Second half of the bug fix: when recovery yields zero
        // classes we must still tell the UI it was a parse error so
        // the browser can distinguish "empty draft" from "broken
        // file" in its empty-state label.
        let (_cs, errors) = parse_classes("model ");
        assert!(errors);
    }

    #[test]
    fn class_kind_variants_round_trip() {
        let src = r#"
model M end M;
block B end B;
connector C end C;
record R end R;
package P end P;
function F end F;
"#;
        let (cs, _errors) = parse_classes(src);
        let kinds: Vec<&ClassType> = cs.iter().map(|c| &c.kind).collect();
        // Don't `use ClassType::*` — `Function` collides with
        // `bevy::reflect::Function` re-exported through other paths.
        assert!(matches!(
            kinds.as_slice(),
            [
                ClassType::Model,
                ClassType::Block,
                ClassType::Connector,
                ClassType::Record,
                ClassType::Package,
                ClassType::Function,
            ]
        ));
    }

    #[test]
    fn fixture_file_parses() {
        let src = include_str!("../../../../assets/models/AnnotatedRocketStage.mo");
        let (cs, _errors) = parse_classes(src);
        // Top level: one package.
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].short_name, "AnnotatedRocketStage");
        assert!(matches!(cs[0].kind, ClassType::Package));
        // Children: RocketStage + Engine + Tank + Gimbal.
        let child_names: Vec<&str> = cs[0]
            .children
            .iter()
            .map(|c| c.short_name.as_str())
            .collect();
        for expected in ["RocketStage", "Engine", "Tank", "Gimbal"] {
            assert!(
                child_names.contains(&expected),
                "missing {expected} (have {child_names:?})"
            );
        }
        // Qualified path correctness.
        assert!(cs[0]
            .children
            .iter()
            .any(|c| c.qualified_path == "AnnotatedRocketStage.Engine"));
    }
}

