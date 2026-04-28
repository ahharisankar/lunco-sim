//! `renderer` — backend-agnostic 2-D drawing API for the diagram canvas.
//!
//! The diagram is painted from many call sites (icon graphics, edge
//! visuals, port markers, junction dots, selection halos, tool
//! previews). Originally each one called egui's `Painter` directly,
//! which welded the rendering layer to egui's CPU tessellator and
//! made experiments with GPU-compute backends (vello) require dual
//! code paths.
//!
//! This module introduces a single trait — [`DiagramRenderer`] —
//! that every paint site goes through. Two implementations exist:
//!
//! - [`egui_backend::EguiRenderer`] — wraps `egui::Painter`. The
//!   path the workbench has shipped on; faithful to today's behaviour.
//! - [`vello_backend::VelloRenderer`] — wraps a `bevy_vello::Scene`.
//!   GPU-compute path raster, supports gradients / smooth strokes /
//!   dirty-region rendering natively.
//!
//! Selection happens at runtime through [`ActiveRenderer`] — an enum
//! that statically dispatches to one of the two implementations. The
//! `match` monomorphises into a single jump per call, so the
//! abstraction cost is in line with hand-written code at the
//! granularity we paint at (~10³ primitives/frame max). A future
//! cargo-feature gate can strip an unused backend from release
//! binaries; the trait shape is the same either way.
//!
//! ## Trait design choices
//!
//! - **Primitive types are owned here**, not borrowed from egui or
//!   peniko. [`Rect`], [`Point`], [`Color`], [`Stroke`] in this
//!   module are the contract; each backend converts at the boundary.
//!   This keeps the upstream code (icon_paint, canvas_diagram)
//!   independent of any one rendering crate.
//! - **Optional advanced primitives** (gradients, cubic bezier
//!   strokes) live on the trait as defaulted methods. Callers that
//!   need a richer effect can probe via [`DiagramRenderer::supports_gradients`]
//!   and degrade gracefully on an egui backend that can't natively
//!   render the effect.
//! - **No retained state on the trait** — every call is a draw
//!   command. Lifetime / borrowing of the underlying painter is
//!   handled inside each backend's `Renderer` struct.
//!
//! ## Migration plan
//!
//! 1. Land the trait + two skeleton backends (this commit).
//! 2. Migrate `icon_paint::paint_rectangle` end-to-end as the
//!    proof point — switch the egui rendering call to
//!    `DiagramRenderer::fill_rect` + `stroke_rect`, run both
//!    backends side-by-side, confirm visual parity.
//! 3. Migrate the rest of icon_paint (lines, polygons, text,
//!    ellipses, bitmaps).
//! 4. Migrate canvas_diagram edge visuals + port markers + arrows.
//! 5. Migrate the lunco-canvas layer system (selection halo,
//!    grid, tool previews).
//! 6. Add the Settings → Renderer toggle, default to Egui until
//!    Vello reaches parity, then flip.

#![allow(dead_code)] // scaffolding — call sites migrate over the next few commits

pub mod egui_backend;
pub mod vello_backend;

// ─── Primitive types ──────────────────────────────────────────────

/// 2-D point in canvas-screen coordinates (pixels). The renderer
/// receives already-transformed positions; world↔screen mapping is
/// the *caller's* responsibility (today via `lunco_canvas::Viewport`,
/// in Phase 2 via a `WorldRenderer` wrapper that owns the transform).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self { Self { x, y } }
}

/// Axis-aligned rectangle. `min` is the top-left, `max` is bottom-
/// right (egui convention — +Y down). Callers that author in
/// Modelica's +Y-up convention must flip before passing in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub min: Point,
    pub max: Point,
}

impl Rect {
    pub fn from_min_max(min: Point, max: Point) -> Self { Self { min, max } }
    pub fn width(&self) -> f32 { self.max.x - self.min.x }
    pub fn height(&self) -> f32 { self.max.y - self.min.y }
}

/// 8-bit straight (un-premultiplied) RGBA. Backends convert to
/// their native representation at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0 };
    pub const WHITE: Self = Self { r: 255, g: 255, b: 255, a: 255 };
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0, a: 255 };
    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    pub const fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
}

/// Stroke style. `width` in pixels, `pattern` controls dashing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stroke {
    pub width: f32,
    pub color: Color,
    pub pattern: LinePattern,
}

impl Stroke {
    pub const NONE: Self = Self {
        width: 0.0,
        color: Color::TRANSPARENT,
        pattern: LinePattern::Solid,
    };
    pub fn solid(width: f32, color: Color) -> Self {
        Self { width, color, pattern: LinePattern::Solid }
    }
    pub fn is_visible(&self) -> bool {
        self.width > 0.0 && self.color.a > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinePattern {
    Solid,
    Dot,
    Dash,
    DashDot,
    DashDotDot,
}

/// Text alignment relative to the supplied position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAnchor {
    LeftTop,
    LeftCenter,
    LeftBottom,
    CenterTop,
    CenterCenter,
    CenterBottom,
    RightTop,
    RightCenter,
    RightBottom,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontStyle {
    /// Pixel size of the line height.
    pub size_px: f32,
    pub bold: bool,
    pub italic: bool,
}

impl FontStyle {
    pub fn proportional(size_px: f32) -> Self {
        Self { size_px, bold: false, italic: false }
    }
}

/// Opaque image handle. Each backend defines what's behind this —
/// the egui backend stores an `egui::TextureId`; the vello backend
/// stores a `Handle<Image>` once we wire a vello image registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageHandle(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientKind {
    /// Linear top→bottom (vertical) gradient inside the rect.
    HorizontalCylinder,
    /// Linear left→right (horizontal) gradient.
    VerticalCylinder,
    /// Radial centred on the rect.
    Sphere,
}

// ─── The trait ────────────────────────────────────────────────────

/// Backend-agnostic immediate-mode 2-D drawing API used by every
/// diagram paint site.
///
/// **Lifecycle**: each frame the host (panel render) builds a
/// renderer, hands it to all paint code, and drops it. Renderers
/// own no scene-level state across frames — they're transient
/// command recorders.
///
/// **Coordinate system**: positions are in pixels relative to the
/// host widget's top-left. Backends apply DPI scaling internally.
pub trait DiagramRenderer {
    fn fill_rect(&mut self, rect: Rect, corner_radius: f32, fill: Color);
    fn stroke_rect(&mut self, rect: Rect, corner_radius: f32, stroke: Stroke);

    fn fill_circle(&mut self, center: Point, radius: f32, fill: Color);
    fn stroke_circle(&mut self, center: Point, radius: f32, stroke: Stroke);

    fn line_segment(&mut self, a: Point, b: Point, stroke: Stroke);

    /// Connected sequence of segments. `closed = true` connects the
    /// last point back to the first. Used for polylines (wires) and
    /// for the outline of a polygon.
    fn polyline(&mut self, pts: &[Point], closed: bool, stroke: Stroke);

    /// Filled convex polygon. Optional outline. Used for arrowheads,
    /// port triangles, authored Modelica `Polygon` primitives.
    fn polygon(&mut self, pts: &[Point], fill: Color, stroke: Stroke);

    /// Filled ellipse inscribed in `rect`. Optional outline.
    fn ellipse(&mut self, rect: Rect, fill: Color, stroke: Stroke);

    /// Single-line text positioned at `pos`, aligned per `anchor`.
    /// Multi-line text is the caller's responsibility (split + emit
    /// once per line) — keeps the trait small.
    fn text(&mut self, pos: Point, anchor: TextAnchor, text: &str, font: FontStyle, color: Color);

    /// Bitmap composited into `rect`. `uv` is the source rectangle in
    /// the texture's normalised coords (`0..1`). `tint` multiplies
    /// every sampled pixel.
    fn image(&mut self, image: ImageHandle, rect: Rect, uv: Rect, tint: Color);

    // ── Optional advanced primitives ──────────────────────────────

    /// True when the backend supports gradient fills natively. Egui
    /// returns `false`; vello returns `true`. Caller logic that
    /// authors gradients can degrade when this returns `false`.
    fn supports_gradients(&self) -> bool { false }

    /// Filled rect with a gradient. Default impl falls back to a
    /// solid fill using the first colour stop. Vello backend
    /// overrides.
    fn fill_gradient(&mut self, rect: Rect, _kind: GradientKind, stops: &[(f32, Color)]) {
        if let Some(&(_, color)) = stops.first() {
            self.fill_rect(rect, 0.0, color);
        }
    }

    /// Cubic Bezier stroke. Default falls back to a polyline with a
    /// fixed sample count.
    fn cubic_bezier(&mut self, p0: Point, p1: Point, p2: Point, p3: Point, stroke: Stroke) {
        const SAMPLES: usize = 32;
        let mut pts = Vec::with_capacity(SAMPLES + 1);
        for i in 0..=SAMPLES {
            let t = i as f32 / SAMPLES as f32;
            let mt = 1.0 - t;
            let x = mt * mt * mt * p0.x
                + 3.0 * mt * mt * t * p1.x
                + 3.0 * mt * t * t * p2.x
                + t * t * t * p3.x;
            let y = mt * mt * mt * p0.y
                + 3.0 * mt * mt * t * p1.y
                + 3.0 * mt * t * t * p2.y
                + t * t * t * p3.y;
            pts.push(Point::new(x, y));
        }
        self.polyline(&pts, false, stroke);
    }
}

// ─── Runtime selection ────────────────────────────────────────────

/// Which backend to use. Persisted in the workbench Settings; users
/// flip this to A/B test without rebuilding. Default during
/// migration is [`Backend::Egui`] (the proven path); flip to
/// [`Backend::Vello`] once parity is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[default]
    Egui,
    Vello,
}

/// Static-dispatch wrapper that owns one of the two backends. The
/// `match` in each method monomorphises into a single jump — same
/// codegen as a hand-rolled if/else, no vtable lookup.
pub enum ActiveRenderer<'a> {
    Egui(egui_backend::EguiRenderer<'a>),
    Vello(vello_backend::VelloRenderer<'a>),
}

impl<'a> DiagramRenderer for ActiveRenderer<'a> {
    fn fill_rect(&mut self, rect: Rect, r: f32, fill: Color) {
        match self {
            Self::Egui(b) => b.fill_rect(rect, r, fill),
            Self::Vello(b) => b.fill_rect(rect, r, fill),
        }
    }
    fn stroke_rect(&mut self, rect: Rect, r: f32, s: Stroke) {
        match self {
            Self::Egui(b) => b.stroke_rect(rect, r, s),
            Self::Vello(b) => b.stroke_rect(rect, r, s),
        }
    }
    fn fill_circle(&mut self, c: Point, r: f32, f: Color) {
        match self {
            Self::Egui(b) => b.fill_circle(c, r, f),
            Self::Vello(b) => b.fill_circle(c, r, f),
        }
    }
    fn stroke_circle(&mut self, c: Point, r: f32, s: Stroke) {
        match self {
            Self::Egui(b) => b.stroke_circle(c, r, s),
            Self::Vello(b) => b.stroke_circle(c, r, s),
        }
    }
    fn line_segment(&mut self, a: Point, b: Point, s: Stroke) {
        match self {
            Self::Egui(r) => r.line_segment(a, b, s),
            Self::Vello(r) => r.line_segment(a, b, s),
        }
    }
    fn polyline(&mut self, pts: &[Point], closed: bool, s: Stroke) {
        match self {
            Self::Egui(r) => r.polyline(pts, closed, s),
            Self::Vello(r) => r.polyline(pts, closed, s),
        }
    }
    fn polygon(&mut self, pts: &[Point], fill: Color, stroke: Stroke) {
        match self {
            Self::Egui(r) => r.polygon(pts, fill, stroke),
            Self::Vello(r) => r.polygon(pts, fill, stroke),
        }
    }
    fn ellipse(&mut self, rect: Rect, fill: Color, stroke: Stroke) {
        match self {
            Self::Egui(r) => r.ellipse(rect, fill, stroke),
            Self::Vello(r) => r.ellipse(rect, fill, stroke),
        }
    }
    fn text(&mut self, pos: Point, anchor: TextAnchor, text: &str, font: FontStyle, color: Color) {
        match self {
            Self::Egui(r) => r.text(pos, anchor, text, font, color),
            Self::Vello(r) => r.text(pos, anchor, text, font, color),
        }
    }
    fn image(&mut self, image: ImageHandle, rect: Rect, uv: Rect, tint: Color) {
        match self {
            Self::Egui(r) => r.image(image, rect, uv, tint),
            Self::Vello(r) => r.image(image, rect, uv, tint),
        }
    }
    fn supports_gradients(&self) -> bool {
        match self {
            Self::Egui(r) => r.supports_gradients(),
            Self::Vello(r) => r.supports_gradients(),
        }
    }
    fn fill_gradient(&mut self, rect: Rect, kind: GradientKind, stops: &[(f32, Color)]) {
        match self {
            Self::Egui(r) => r.fill_gradient(rect, kind, stops),
            Self::Vello(r) => r.fill_gradient(rect, kind, stops),
        }
    }
    fn cubic_bezier(&mut self, p0: Point, p1: Point, p2: Point, p3: Point, stroke: Stroke) {
        match self {
            Self::Egui(r) => r.cubic_bezier(p0, p1, p2, p3, stroke),
            Self::Vello(r) => r.cubic_bezier(p0, p1, p2, p3, stroke),
        }
    }
}
