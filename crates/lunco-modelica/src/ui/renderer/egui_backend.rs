//! Egui implementation of [`crate::ui::renderer::DiagramRenderer`].
//!
//! Wraps an `egui::Painter` and translates each trait call into the
//! corresponding `Painter` method. This is the backend the workbench
//! has shipped on for years; the migration target is for it to keep
//! producing pixel-identical output to the pre-trait code.

use bevy_egui::egui;
use super::{
    Color, DiagramRenderer, FontStyle, ImageHandle, LinePattern, Point, Rect,
    Stroke, TextAnchor,
};

pub struct EguiRenderer<'a> {
    painter: &'a egui::Painter,
}

impl<'a> EguiRenderer<'a> {
    pub fn new(painter: &'a egui::Painter) -> Self {
        Self { painter }
    }

    /// Borrow the wrapped painter for direct egui access — escape
    /// hatch for primitives that haven't been migrated yet.
    pub fn painter(&self) -> &egui::Painter {
        self.painter
    }
}

fn to_pos2(p: Point) -> egui::Pos2 {
    egui::pos2(p.x, p.y)
}

fn to_rect(r: Rect) -> egui::Rect {
    egui::Rect::from_min_max(to_pos2(r.min), to_pos2(r.max))
}

fn to_color32(c: Color) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

fn to_stroke(s: Stroke) -> egui::Stroke {
    if !s.is_visible() {
        return egui::Stroke::NONE;
    }
    // LinePattern is approximated as a solid stroke for now —
    // dashing in egui needs a manual segmentation pass that the
    // current canvas_diagram code does explicitly. Once we migrate
    // that loop here, expose dashed strokes through a wrapper.
    egui::Stroke::new(s.width, to_color32(s.color))
}

impl<'a> DiagramRenderer for EguiRenderer<'a> {
    fn fill_rect(&mut self, rect: Rect, corner_radius: f32, fill: Color) {
        if fill.a == 0 {
            return;
        }
        self.painter.rect_filled(to_rect(rect), corner_radius, to_color32(fill));
    }

    fn stroke_rect(&mut self, rect: Rect, corner_radius: f32, stroke: Stroke) {
        if !stroke.is_visible() {
            return;
        }
        self.painter.rect_stroke(
            to_rect(rect),
            corner_radius,
            to_stroke(stroke),
            egui::StrokeKind::Inside,
        );
    }

    fn fill_circle(&mut self, center: Point, radius: f32, fill: Color) {
        if fill.a == 0 {
            return;
        }
        self.painter.circle_filled(to_pos2(center), radius, to_color32(fill));
    }

    fn stroke_circle(&mut self, center: Point, radius: f32, stroke: Stroke) {
        if !stroke.is_visible() {
            return;
        }
        self.painter
            .circle_stroke(to_pos2(center), radius, to_stroke(stroke));
    }

    fn line_segment(&mut self, a: Point, b: Point, stroke: Stroke) {
        if !stroke.is_visible() {
            return;
        }
        self.painter
            .line_segment([to_pos2(a), to_pos2(b)], to_stroke(stroke));
    }

    fn polyline(&mut self, pts: &[Point], closed: bool, stroke: Stroke) {
        if !stroke.is_visible() || pts.len() < 2 {
            return;
        }
        let pts2: Vec<egui::Pos2> = pts.iter().copied().map(to_pos2).collect();
        // egui's `Shape::line` is open by default; for a closed
        // path we feed back the first point at the end.
        let mut pts2 = pts2;
        if closed {
            pts2.push(pts2[0]);
        }
        self.painter.add(egui::Shape::line(pts2, to_stroke(stroke)));
    }

    fn polygon(&mut self, pts: &[Point], fill: Color, stroke: Stroke) {
        if pts.len() < 3 {
            return;
        }
        let pts2: Vec<egui::Pos2> = pts.iter().copied().map(to_pos2).collect();
        self.painter.add(egui::Shape::convex_polygon(
            pts2,
            to_color32(fill),
            to_stroke(stroke),
        ));
    }

    fn ellipse(&mut self, rect: Rect, fill: Color, stroke: Stroke) {
        // Egui paints ellipses by tessellating into a polygon at
        // sample density tuned for the on-screen size. Mirrors
        // what icon_paint::paint_ellipse does today.
        let (cx, cy) = (
            (rect.min.x + rect.max.x) * 0.5,
            (rect.min.y + rect.max.y) * 0.5,
        );
        let (rx, ry) = (rect.width() * 0.5, rect.height() * 0.5);
        let r_max = rx.abs().max(ry.abs());
        let segs = ((r_max * std::f32::consts::TAU).round() as usize).clamp(24, 96);
        let mut pts: Vec<egui::Pos2> = Vec::with_capacity(segs);
        for i in 0..segs {
            let theta = (i as f32) * std::f32::consts::TAU / (segs as f32);
            pts.push(egui::pos2(cx + rx * theta.cos(), cy + ry * theta.sin()));
        }
        if fill.a > 0 {
            self.painter.add(egui::Shape::convex_polygon(
                pts.clone(),
                to_color32(fill),
                to_stroke(Stroke {
                    pattern: LinePattern::Solid,
                    ..stroke
                }),
            ));
        } else if stroke.is_visible() {
            pts.push(pts[0]);
            self.painter.add(egui::Shape::line(pts, to_stroke(stroke)));
        }
    }

    fn text(&mut self, pos: Point, anchor: TextAnchor, text: &str, font: FontStyle, color: Color) {
        let align = match anchor {
            TextAnchor::LeftTop => egui::Align2::LEFT_TOP,
            TextAnchor::LeftCenter => egui::Align2::LEFT_CENTER,
            TextAnchor::LeftBottom => egui::Align2::LEFT_BOTTOM,
            TextAnchor::CenterTop => egui::Align2::CENTER_TOP,
            TextAnchor::CenterCenter => egui::Align2::CENTER_CENTER,
            TextAnchor::CenterBottom => egui::Align2::CENTER_BOTTOM,
            TextAnchor::RightTop => egui::Align2::RIGHT_TOP,
            TextAnchor::RightCenter => egui::Align2::RIGHT_CENTER,
            TextAnchor::RightBottom => egui::Align2::RIGHT_BOTTOM,
        };
        // Bold/italic require font ID lookup against the egui style;
        // the workbench installs only Proportional + Monospace
        // families. Use Proportional and ignore italic/bold flags
        // until we wire styled fonts (Phase 2).
        let font_id = egui::FontId::proportional(font.size_px);
        self.painter
            .text(to_pos2(pos), align, text, font_id, to_color32(color));
    }

    fn image(&mut self, image: ImageHandle, rect: Rect, uv: Rect, tint: Color) {
        // The trait's opaque ImageHandle stores an egui TextureId
        // bit-cast into u64 — set by callers that obtained the id
        // from `EguiUserTextures`. Two TextureId variants exist
        // (User(u64) and Managed(u64)); we keep User-only for now.
        let texture_id = egui::TextureId::User(image.0);
        self.painter
            .image(texture_id, to_rect(rect), to_rect(uv), to_color32(tint));
    }
}
