//! API handlers for class-level operations (Rename, etc).

use bevy::prelude::*;
use lunco_core::{Command, on_command};
use lunco_doc::DocumentId;
use crate::document::ModelicaOp;
use crate::ui::state::ModelicaDocumentRegistry;
use super::util::resolve_doc;

/// Rename a top-level class within an open Modelica document.
#[Command(default)]
pub struct RenameModelicaClass {
    pub doc: DocumentId,
    pub old_name: String,
    pub new_name: String,
}

#[on_command(RenameModelicaClass)]
pub fn on_rename_modelica_class(
    trigger: On<RenameModelicaClass>,
    mut commands: Commands,
) {
    let ev = trigger.event().clone();
    commands.queue(move |world: &mut World| {
        let Some(doc) = resolve_doc(world, ev.doc) else {
            bevy::log::warn!("[RenameModelicaClass] no doc for id {}", ev.doc);
            return;
        };
        if ev.old_name.is_empty() || ev.new_name.is_empty() {
            bevy::log::warn!("[RenameModelicaClass] old/new must be non-empty");
            return;
        }
        if !ev.new_name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            bevy::log::warn!(
                "[RenameModelicaClass] new_name `{}` must be a valid identifier",
                ev.new_name
            );
            return;
        }

        let registry = world.resource::<ModelicaDocumentRegistry>();
        let Some(host) = registry.host(doc) else {
            return;
        };
        let source = host.document().source().to_string();
        let new_source = match rewrite_class_name(&source, &ev.old_name, &ev.new_name) {
            Some(s) => s,
            None => {
                bevy::log::warn!(
                    "[RenameModelicaClass] no `<keyword> {}` declaration found in doc {}",
                    ev.old_name,
                    doc.raw()
                );
                return;
            }
        };

        match crate::ui::panels::canvas_diagram::apply_one_op_as(
            world,
            doc,
            ModelicaOp::ReplaceSource { new: new_source },
            lunco_twin_journal::AuthorTag::for_tool("api"),
        ) {
            Ok(_) => {}
            Err(e) => {
                bevy::log::warn!(
                    "[RenameModelicaClass] doc={} apply failed: {:?}",
                    doc.raw(),
                    e
                );
                return;
            }
        }

        if let Some(mut registry) = world.get_resource_mut::<ModelicaDocumentRegistry>() {
            if let Some(host) = registry.host_mut(doc) {
                let doc_obj = host.document_mut();
                if doc_obj.origin().is_untitled() {
                    doc_obj.set_origin(lunco_doc::DocumentOrigin::untitled(ev.new_name.clone()));
                }
            }
        }
        bevy::log::info!(
            "[RenameModelicaClass] doc={} {} → {}",
            doc.raw(),
            ev.old_name,
            ev.new_name
        );
    });
}

fn rewrite_class_name(source: &str, old: &str, new: &str) -> Option<String> {
    const KEYWORDS: &[&str] = &[
        "model", "class", "package", "connector", "record", "block", "type", "function",
    ];
    let bytes = source.as_bytes();
    let mut decl_pos = None;
    let mut decl_len = 0;
    let mut decl_kw = "";
    'outer: for (i, _) in source.char_indices() {
        for kw in KEYWORDS {
            let pat_len = kw.len() + 1 + old.len();
            if i + pat_len > source.len() { continue; }
            if !source[i..].starts_with(kw) { continue; }
            if bytes[i + kw.len()] != b' ' { continue; }
            if !source[i + kw.len() + 1..].starts_with(old) { continue; }
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = i + pat_len;
            let after_ok = after >= bytes.len() || !is_ident_byte(bytes[after]);
            if before_ok && after_ok {
                decl_pos = Some(i);
                decl_len = pat_len;
                decl_kw = kw;
                break 'outer;
            }
        }
    }
    let pos = decl_pos?;
    let mut out = String::with_capacity(source.len() + new.len());
    out.push_str(&source[..pos]);
    out.push_str(&format!("{decl_kw} {new}"));
    out.push_str(&source[pos + decl_len..]);
    let end_pat = format!("end {old};");
    let new_end = format!("end {new};");
    Some(out.replacen(&end_pat, &new_end, 1))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}
