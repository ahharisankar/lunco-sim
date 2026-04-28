//! Vello implementation of [`crate::ui::renderer::DiagramRenderer`].
//!
//! Wraps a `bevy_vello::prelude::VelloScene2d` reference and emits
//! kurbo paths / peniko fills + strokes per call. Vello renders the
//! scene through its GPU compute pipeline; the host (the
//! `vello_canvas` plugin) owns the camera + render target and does
//! not appear here.
//!
//! Status: scaffold. All trait methods compile; only `fill_rect` is
//! semantically migrated as the proof point. The rest are stubs that
//! draw placeholder geometry until each migration commit lands.

use bevy_vello::prelude::VelloScene2d;
use bevy_vello::vello::{
    kurbo::{
        Affine, BezPath, Circle as KurboCircle, Point as KurboPoint,
        Rect as KurboRect, RoundedRect, Stroke as KurboStroke,
    },
    peniko::{Color as PenikoColor, Fill},
};

use super::{
    Color, DiagramRenderer, FontStyle, ImageHandle, LinePattern, Point, Rect,
    Stroke, TextAnchor,
};

pub struct VelloRenderer<'a> {
    scene: &'a mut VelloScene2d,
    /// World transform applied before each path is submitted.
    /// Today: `Affine::IDENTITY` + screen-space coords. Phase 2
    /// will plumb the per-tab Viewport's pan + zoom in here so
    /// the abstraction stays renderer-agnostic.
    transform: Affine,
}

impl<'a> VelloRenderer<'a> {
    pub fn new(scene: &'a mut VelloScene2d) -> Self {
        Self { scene, transform: Affine::IDENTITY }
    }

    /// Override the world→screen transform applied to subsequent
    /// primitives. Set once per frame from the host's viewport.
    pub fn with_transform(scene: &'a mut VelloScene2d, transform: Affine) -> Self {
        Self { scene, transform }
    }

    pub fn scene_mut(&mut self) -> &mut VelloScene2d { self.scene }
}

fn to_kurbo_point(p: Point) -> KurboPoint {
    KurboPoint::new(p.x as f64, p.y as f64)
}

fn to_kurbo_rect(r: Rect) -> KurboRect {
    KurboRect::new(
        r.min.x as f64,
        r.min.y as f64,
        r.max.x as f64,
        r.max.y as f64,
    )
}

fn to_peniko_color(c: Color) -> PenikoColor {
    PenikoColor::new([
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        c.a as f32 / 255.0,
    ])
}

fn to_kurbo_stroke(s: Stroke) -> KurboStroke {
    let mut k = KurboStroke::new(s.width as f64);
    if let Some(pat) = dash_pattern(s.pattern, s.width) {
        k = k.with_dashes(0.0, pat);
    }
    k
}

fn dash_pattern(p: LinePattern, w: f32) -> Option<Vec<f64>> {
    let u = w.max(1.0) as f64;
    match p {
        LinePattern::Solid => None,
        LinePattern::Dot => Some(vec![u * 1.0, u * 2.0]),
        LinePattern::Dash => Some(vec![u * 4.0, u * 2.0]),
        LinePattern::DashDot => Some(vec![u * 4.0, u * 2.0, u * 1.0, u * 2.0]),
        LinePattern::DashDotDot => {
            Some(vec![u * 4.0, u * 2.0, u * 1.0, u * 2.0, u * 1.0, u * 2.0])
        }
    }
}

impl<'a> DiagramRenderer for VelloRenderer<'a> {
    fn fill_rect(&mut self, rect: Rect, corner_radius: f32, fill: Color) {
        if fill.a == 0 { return; }
        let r = to_kurbo_rect(rect);
        if corner_radius > 0.0 {
            let rr = RoundedRect::from_rect(r, corner_radius as f64);
            self.scene.fill(Fill::NonZero, self.transform, to_peniko_color(fill), None, &rr);
        } else {
            self.scene.fill(Fill::NonZero, self.transform, to_peniko_color(fill), None, &r);
        }
    }

    fn stroke_rect(&mut self, rect: Rect, corner_radius: f32, stroke: Stroke) {
        if !stroke.is_visible() { return; }
        let r = to_kurbo_rect(rect);
        let pen = to_peniko_color(stroke.color);
        let kstroke = to_kurbo_stroke(stroke);
        if corner_radius > 0.0 {
            let rr = RoundedRect::from_rect(r, corner_radius as f64);
            self.scene.stroke(&kstroke, self.transform, pen, None, &rr);
        } else {
            self.scene.stroke(&kstroke, self.transform, pen, None, &r);
        }
    }

    fn fill_circle(&mut self, center: Point, radius: f32, fill: Color) {
        if fill.a == 0 { return; }
        let c = KurboCircle::new(to_kurbo_point(center), radius as f64);
        self.scene.fill(Fill::NonZero, self.transform, to_peniko_color(fill), None, &c);
    }

    fn stroke_circle(&mut self, center: Point, radius: f32, stroke: Stroke) {
        if !stroke.is_visible() { return; }
        let c = KurboCircle::new(to_kurbo_point(center), radius as f64);
        self.scene.stroke(
            &to_kurbo_stroke(stroke),
            self.transform,
            to_peniko_color(stroke.color),
            None,
            &c,
        );
    }

    fn line_segment(&mut self, a: Point, b: Point, stroke: Stroke) {
        if !stroke.is_visible() { return; }
        let mut path = BezPath::new();
        path.move_to(to_kurbo_point(a));
        path.line_to(to_kurbo_point(b));
        self.scene.stroke(
            &to_kurbo_stroke(stroke),
            self.transform,
            to_peniko_color(stroke.color),
            None,
            &path,
        );
    }

    fn polyline(&mut self, pts: &[Point], closed: bool, stroke: Stroke) {
        if !stroke.is_visible() || pts.len() < 2 { return; }
        let mut path = BezPath::new();
        path.move_to(to_kurbo_point(pts[0]));
        for p in &pts[1..] {
            path.line_to(to_kurbo_point(*p));
        }
        if closed { path.close_path(); }
        self.scene.stroke(
            &to_kurbo_stroke(stroke),
            self.transform,
            to_peniko_color(stroke.color),
            None,
            &path,
        );
    }

    fn polygon(&mut self, pts: &[Point], fill: Color, stroke: Stroke) {
        if pts.len() < 3 { return; }
        let mut path = BezPath::new();
        path.move_to(to_kurbo_point(pts[0]));
        for p in &pts[1..] {
            path.line_to(to_kurbo_point(*p));
        }
        path.close_path();
        if fill.a > 0 {
            self.scene.fill(
                Fill::NonZero,
                self.transform,
                to_peniko_color(fill),
                None,
                &path,
            );
        }
        if stroke.is_visible() {
            self.scene.stroke(
                &to_kurbo_stroke(stroke),
                self.transform,
                to_peniko_color(stroke.color),
                None,
                &path,
            );
        }
    }

    fn ellipse(&mut self, rect: Rect, fill: Color, stroke: Stroke) {
        // Vello's kurbo carries an `Ellipse` primitive — direct path.
        use bevy_vello::vello::kurbo::Ellipse;
        let (cx, cy) = (
            (rect.min.x + rect.max.x) as f64 * 0.5,
            (rect.min.y + rect.max.y) as f64 * 0.5,
        );
        let (rx, ry) = (
            rect.width() as f64 * 0.5,
            rect.height() as f64 * 0.5,
        );
        let e = Ellipse::new((cx, cy), (rx, ry), 0.0);
        if fill.a > 0 {
            self.scene.fill(
                Fill::NonZero,
                self.transform,
                to_peniko_color(fill),
                None,
                &e,
            );
        }
        if stroke.is_visible() {
            self.scene.stroke(
                &to_kurbo_stroke(stroke),
                self.transform,
                to_peniko_color(stroke.color),
                None,
                &e,
            );
        }
    }

    fn text(&mut self, _pos: Point, _anchor: TextAnchor, _text: &str, _font: FontStyle, _color: Color) {
        // Vello text needs the optional `text` feature + a font
        // database. Phase 2 wires our existing Noto/DejaVu fallback
        // through `vello::skrifa`; for now we silently skip.
    }

    fn image(&mut self, _image: ImageHandle, _rect: Rect, _uv: Rect, _tint: Color) {
        // Vello image rendering needs a separate image registry
        // mapping our `ImageHandle` to vello `Image` resources;
        // wired in Phase 2 alongside the bitmap migration.
    }

    fn supports_gradients(&self) -> bool { false }

}
