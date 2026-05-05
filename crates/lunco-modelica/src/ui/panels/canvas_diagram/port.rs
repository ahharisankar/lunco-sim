//! Port helpers for the canvas diagram panel.
//!
//! Leaf utilities for serialising port kinds, painting per-port shape
//! markers, rendering Dashboard-style input-control widgets on top of
//! component icons, computing fallback port offsets when an authored
//! `Placement` is missing, and resolving a port's connector-class
//! `Icon` from the MSL palette / engine.

use std::collections::HashMap;

use bevy_egui::egui;

use super::edge::PortDir;

/// Serialise a [`PortKind`](crate::visual_diagram::PortKind) into the
/// short string used in edge JSON data, so the factory can round-trip
/// it without pulling in serde enum tagging.
pub(super) fn port_kind_str(kind: crate::visual_diagram::PortKind) -> &'static str {
    match kind {
        crate::visual_diagram::PortKind::Input => "input",
        crate::visual_diagram::PortKind::Output => "output",
        crate::visual_diagram::PortKind::Acausal => "acausal",
    }
}

/// Render Dashboard-style in-canvas control widgets for every
/// bounded input attached to this instance. Mirrors the Simulink
/// `Dashboard.Slider` / SCADA HMI pattern: small interactive strips
/// rendered ON the node, separate from the icon body, that capture
/// pointer events directly so dragging a slider doesn't also drag
/// the node.
///
/// Coverage:
/// - One vertical strip per input whose key starts with `<instance>.`
///   *and* has finite declared `min`/`max` bounds.
/// - Strips are stacked side by side along the right edge of the
///   icon, leaf-name tooltip on hover so users learn which strip
///   controls which input.
/// - Inputs without bounds are skipped (we'd have nothing to map
///   drag distance against). A future revision can add a
///   knob-style relative-drag widget for unbounded inputs and an
///   explicit `__LunCo_inputControl(target=...)` annotation for
///   model authors who want fine control over placement / kind.
pub(super) fn paint_input_control_widget(
    ui: &mut egui::Ui,
    icon_rect: egui::Rect,
    instance_name: &str,
    zoom: f32,
) {
    let _ = zoom;
    if instance_name.is_empty() || icon_rect.height() < 24.0 {
        return;
    }
    let snap = lunco_viz::kinds::canvas_plot_node::fetch_input_control_snapshot(ui.ctx());
    let prefix = format!("{instance_name}.");

    let mut bound: Vec<(String, f64, f64, f64)> = snap
        .inputs
        .iter()
        .filter(|(name, _)| name.starts_with(&prefix))
        .filter_map(|(name, (value, min, max))| {
            let (mn, mx) = (min.as_ref()?, max.as_ref()?);
            if mx <= mn {
                return None;
            }
            Some((name.clone(), *value, *mn, *mx))
        })
        .collect();
    if bound.is_empty() {
        return;
    }
    bound.sort_by(|a, b| a.0.cmp(&b.0));

    let strip_width = (icon_rect.height() * 0.08).max(4.0);
    let strip_gap = strip_width * 0.4;
    let strip_pad = strip_width * 0.5;
    let h = icon_rect.height() * 0.7;
    let s = strip_width / 10.0;
    let strip_top_y = icon_rect.center().y - h * 0.5;

    for (idx, (name, value, mn, mx)) in bound.iter().enumerate() {
        let x = icon_rect.right()
            - strip_pad
            - strip_width
            - (idx as f32) * (strip_width + strip_gap);
        let strip_rect = egui::Rect::from_min_size(
            egui::pos2(x, strip_top_y),
            egui::vec2(strip_width, h),
        );

        lunco_canvas::canvas::push_canvas_widget_rect(ui.ctx(), strip_rect);

        let trough_color = egui::Color32::from_rgba_unmultiplied(28, 30, 38, 220);
        let radius = (strip_width * 0.45).min(5.0);
        ui.painter().rect_filled(strip_rect, radius, trough_color);

        let frac = ((*value - *mn) / (*mx - *mn)).clamp(0.0, 1.0) as f32;
        if frac > 0.0 {
            let fill_h = strip_rect.height() * frac;
            let fill_rect = egui::Rect::from_min_size(
                egui::pos2(strip_rect.min.x, strip_rect.max.y - fill_h),
                egui::vec2(strip_rect.width(), fill_h),
            );
            let fill_color = egui::Color32::from_rgb(70, 160, 240);
            ui.painter().rect_filled(fill_rect, radius, fill_color);
            let y = strip_rect.max.y - fill_h;
            ui.painter().line_segment(
                [egui::pos2(strip_rect.min.x, y), egui::pos2(strip_rect.max.x, y)],
                egui::Stroke::new(1.5 * s, egui::Color32::from_rgb(220, 235, 250)),
            );
        }

        ui.painter().rect_stroke(
            strip_rect,
            radius,
            egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 130, 145)),
            egui::StrokeKind::Inside,
        );

        let widget_id = egui::Id::new(("lunco_input_control", name.clone()));
        let response = ui.interact(strip_rect, widget_id, egui::Sense::click_and_drag());
        if response.dragged() || response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let y_rel = (pos.y - strip_rect.min.y) / strip_rect.height();
                let inv = (1.0 - y_rel).clamp(0.0, 1.0) as f64;
                let new_value = mn + inv * (mx - mn);
                if (new_value - value).abs() > 1e-9 {
                    lunco_viz::kinds::canvas_plot_node::queue_input_write(
                        ui.ctx(),
                        name,
                        new_value,
                    );
                }
            }
        }
        if response.hovered() {
            let leaf = name.rsplit('.').next().unwrap_or(name);
            let tooltip = if (mx - mn - 100.0).abs() < 1e-6 && mn.abs() < 1e-6 {
                format!("{leaf}: {value:.1} %")
            } else {
                format!("{leaf}: {value:.3} (range {mn:.2} … {mx:.2})")
            };
            response.on_hover_text(tooltip);
        }
    }
}

/// Visual style of a port marker on a component icon. Mirrors the
/// OMEdit / Dymola convention so users can read connector causality
/// at a glance without hovering for the type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PortShape {
    /// Filled square — `input` causality (RealInput, BooleanInput, …).
    InputSquare,
    /// Filled triangle pointing outward from the icon — `output`
    /// causality (RealOutput, BooleanOutput, …).
    OutputTriangle,
    /// Filled circle — acausal physical connectors (Pin, Flange, …).
    AcausalCircle,
}

/// Paint a port marker at `center` using the OMEdit shape convention
/// described on [`PortShape`]. `dir` orients the output triangle so
/// it points away from the icon body; ignored for square / circle.
#[allow(non_snake_case)]
pub(super) fn paint_port_shape(
    painter: &egui::Painter,
    center: egui::Pos2,
    shape: PortShape,
    dir: PortDir,
    fill: egui::Color32,
    stroke: egui::Stroke,
    scale: f32,
) {
    let r: f32 = 1.4 * scale;
    let R = r;
    match shape {
        PortShape::InputSquare => {
            let rect = egui::Rect::from_center_size(center, egui::vec2(R * 1.6, R * 1.6));
            painter.rect_filled(rect, 0.0, fill);
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
        }
        PortShape::OutputTriangle => {
            let (ox, oy) = dir.outward();
            if (ox, oy) == (0.0, 0.0) {
                let rect = egui::Rect::from_center_size(
                    center,
                    egui::vec2(R * 1.6, R * 1.6),
                );
                painter.rect_filled(rect, 0.0, fill);
                painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);
                return;
            }
            let (px, py) = (-oy, ox);
            let tip = egui::pos2(center.x + ox * R * 1.4, center.y + oy * R * 1.4);
            let b1 = egui::pos2(
                center.x - ox * R * 0.4 + px * R * 0.9,
                center.y - oy * R * 0.4 + py * R * 0.9,
            );
            let b2 = egui::pos2(
                center.x - ox * R * 0.4 - px * R * 0.9,
                center.y - oy * R * 0.4 - py * R * 0.9,
            );
            let pts = vec![tip, b1, b2];
            painter.add(egui::Shape::convex_polygon(pts.clone(), fill, stroke));
        }
        PortShape::AcausalCircle => {
            painter.circle_filled(center, R - 1.0, fill);
            painter.circle_stroke(center, R - 1.0, stroke);
        }
    }
}

/// Same fallback layout as `port_fallback_offset` but parameterised by
/// the icon's actual width/height — needed once Placement-driven node
/// sizing makes per-instance dimensions vary instead of always being
/// 20×20.
pub(super) fn port_fallback_offset_for_size(
    index: usize,
    _total: usize,
    icon_w: f32,
    icon_h: f32,
) -> (f32, f32) {
    let side_left = index % 2 == 0;
    let row = index / 2;
    let cy = icon_h * 0.5 - (row as f32) * (icon_h * 0.25);
    let cx = if side_left { 0.0 } else { icon_w };
    (cx, cy.clamp(0.0, icon_h))
}

/// Resolve each port's connector-class `Icon` using the engine's
/// indexed view, mirroring the candidate-path walk that the painter
/// previously did inline. Runs off-thread inside the projection task,
/// so the engine lock is taken once per port (typically 2–8 per
/// component) at projection time — never during paint.
///
/// Empty `msl_path` (port type not classified yet) → `None`.
/// Qualified path that fails to resolve → walk parent's package
/// chain trying `<pkg>.Interfaces.<name>` and `<pkg>.<name>` so
/// older indexes that wrote unqualified types still find their
/// connector class.
pub(super) fn resolve_port_icons(
    parent_qualified: &str,
    ports: &[crate::visual_diagram::PortDef],
) -> Vec<Option<crate::annotations::Icon>> {
    let palette = crate::visual_diagram::msl_component_library();
    let palette_lookup: HashMap<&str, &crate::visual_diagram::MSLComponentDef> =
        palette.iter().map(|d| (d.msl_path.as_str(), d)).collect();
    let handle = crate::engine_resource::global_engine_handle();
    ports
        .iter()
        .map(|p| {
            let path = &p.msl_path;
            let candidates: Vec<String> = if path.contains('.') {
                vec![path.clone()]
            } else if !path.is_empty() {
                let mut out = Vec::new();
                let mut scope = parent_qualified.to_string();
                while scope.contains('.') {
                    let pkg = scope.rsplitn(2, '.').nth(1).unwrap_or("").to_string();
                    if !pkg.is_empty() {
                        out.push(format!("{pkg}.Interfaces.{path}"));
                        out.push(format!("{pkg}.{path}"));
                    }
                    scope = pkg;
                }
                out.push(path.clone());
                out
            } else {
                return None;
            };
            for c in &candidates {
                if let Some(def) = palette_lookup.get(c.as_str()) {
                    if let Some(icon) = def.icon_graphics.as_ref() {
                        return Some(icon.clone());
                    }
                }
                if let Some(handle) = handle.as_ref() {
                    let mut engine = handle.lock();
                    if let Some(icon) = engine.icon_for(c) {
                        return Some(icon);
                    }
                    if let Some(cd) = engine.class_def(c) {
                        if let Some(icon) = crate::annotations::extract_icon(&cd.annotation) {
                            return Some(icon);
                        }
                    }
                }
            }
            None
        })
        .collect()
}
