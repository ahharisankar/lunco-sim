//! Scope-driven loading indicator widget.
//!
//! Panels build a `LoadingIndicator::for_scope(scope)` and call one of
//! the render flavours (`overlay_on`, `inline`, `banner`). The widget
//! reads [`StatusBus`] to decide whether to paint and what to paint —
//! call sites do not pick visuals, they declare scope.

use std::time::Duration;

use bevy_egui::egui::{self, Align2, Color32, FontId, Rect, Vec2};
use lunco_theme::Theme;
use web_time::Instant;

use super::spinner::paint_three_dot;
use super::{BusyScope, StatusBus, StatusEvent};

/// Wait before showing an indicator. Below this elapsed time we paint
/// nothing — fast paths never flicker a spinner. Best-practice band is
/// 100–250 ms; pick the conservative end so a freshly-clicked drill-in
/// has a moment to resolve from cache.
pub(crate) const SHOW_AFTER: Duration = Duration::from_millis(200);

/// Threshold past which the overlay shows an elapsed-time read-out.
/// Below this the user is unlikely to want a precise number; above it
/// they're starting to wonder if the task is wedged.
pub(crate) const ELAPSED_AFTER: Duration = Duration::from_secs(3);

/// Builder for a scope-driven indicator. Call one of the render
/// flavours to paint; if the scope is not busy or has not yet crossed
/// [`SHOW_AFTER`], the call is a no-op.
pub struct LoadingIndicator {
    scope: BusyScope,
}

impl LoadingIndicator {
    /// Indicator that paints when anything within `scope` is busy.
    pub fn for_scope(scope: BusyScope) -> Self {
        Self { scope }
    }

    /// Resolve the entry to render — the longest-running busy entry
    /// in scope whose elapsed time has crossed [`SHOW_AFTER`].
    fn pick<'a>(&self, bus: &'a StatusBus) -> Option<&'a StatusEvent> {
        let ev = bus.longest_in(self.scope)?;
        if Instant::now().saturating_duration_since(ev.at) < SHOW_AFTER {
            return None;
        }
        Some(ev)
    }

    /// Centred card painted over `rect` — the canvas-overlay flavour.
    /// No-op when the scope is not busy.
    pub fn overlay_on(self, ui: &mut egui::Ui, rect: Rect, bus: &StatusBus, theme: &Theme) {
        let Some(ev) = self.pick(bus) else { return };
        let painter = ui.painter_at(rect);

        let card_size = Vec2::new(220.0, 90.0);
        let card_rect = Rect::from_center_size(rect.center(), card_size);
        let scrim = Color32::from_rgba_unmultiplied(0, 0, 0, 110);
        painter.rect_filled(rect, 0.0, scrim);
        painter.rect_filled(card_rect, 8.0, theme.colors.surface0);

        let dots_centre = card_rect.center() - Vec2::new(0.0, 14.0);
        // Repaint on the host ui (painter_at clips repaint requests).
        paint_three_dot(ui, dots_centre, theme.colors.text);

        let label = if !ev.message.is_empty() {
            ev.message.as_str()
        } else {
            "Loading…"
        };
        painter.text(
            card_rect.center() + Vec2::new(0.0, 18.0),
            Align2::CENTER_CENTER,
            label,
            FontId::proportional(13.0),
            theme.colors.text,
        );

        let elapsed = Instant::now().saturating_duration_since(ev.at);
        if elapsed >= ELAPSED_AFTER {
            painter.text(
                card_rect.center_bottom() - Vec2::new(0.0, 10.0),
                Align2::CENTER_CENTER,
                format!("{:.1}s", elapsed.as_secs_f32()),
                FontId::proportional(11.0),
                theme.colors.subtext0,
            );
        }
    }

    /// Inline indicator suitable for a tree row (no scrim, no card).
    /// Renders as `⌛ <label>` in a muted colour. No-op when not busy.
    pub fn inline(self, ui: &mut egui::Ui, bus: &StatusBus, theme: &Theme) {
        let Some(ev) = self.pick(bus) else { return };
        let label = if ev.message.is_empty() {
            "Loading…".to_string()
        } else {
            format!("⌛ {}", ev.message)
        };
        ui.colored_label(theme.colors.subtext0, label);
        ui.ctx().request_repaint();
    }

    /// Top-of-panel banner suitable for document-scope work. No-op
    /// when the scope is not busy. Reserved for Phase 3+ writers.
    pub fn banner(self, ui: &mut egui::Ui, bus: &StatusBus, theme: &Theme) {
        let Some(ev) = self.pick(bus) else { return };
        egui::Frame::new()
            .fill(theme.colors.surface0)
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("⌛").color(theme.colors.text));
                    let label = if ev.message.is_empty() {
                        "Loading…"
                    } else {
                        ev.message.as_str()
                    };
                    ui.colored_label(theme.colors.text, label);
                });
            });
        ui.ctx().request_repaint();
    }
}
