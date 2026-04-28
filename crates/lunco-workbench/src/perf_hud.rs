//! Performance HUD for the status bar.
//!
//! Off by default. Persisted via `lunco-settings` (one shared
//! `~/.lunco/settings.json`) so the user's choice survives restarts.
//! Three ways to flip it:
//!
//! - **Settings menu** — `Settings ▸ Performance HUD` checkbox.
//! - **Typed command** — [`TogglePerfHud`] over the API/script bus.
//! - **Direct mutation** — write to [`PerfHudSettings::enabled`].
//!
//! Live samples (`fps`, `frame_ms`, `physics_ms`) live on a separate
//! [`PerfStats`] resource — those don't belong in persistable
//! settings. The status bar reads from `PerfStats` for the numbers
//! and from `PerfHudSettings.enabled` for visibility.
//!
//! Workbench itself stays physics-agnostic: `physics_ms` is a plain
//! `Option<f32>` that another crate (e.g. `lunco-sandbox-edit`)
//! populates when avian is in the build.

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;
use lunco_core::{Command, on_command, register_commands};
use lunco_settings::{AppSettingsExt, SettingsSection};
use serde::{Deserialize, Serialize};

/// Persisted user preference for the perf HUD. Stored under the
/// `"perf_hud"` key of `settings.json`.
#[derive(Resource, Serialize, Deserialize, Default, Clone, Copy, PartialEq, Debug)]
pub struct PerfHudSettings {
    /// Whether the HUD shows in the status bar.
    pub enabled: bool,
}

impl SettingsSection for PerfHudSettings {
    const KEY: &'static str = "perf_hud";
}

/// Live, per-frame perf samples. Not persisted — these are reset
/// when the HUD is disabled and resampled while it's on.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PerfStats {
    /// Smoothed FPS from Bevy's `FrameTimeDiagnosticsPlugin`.
    pub fps: f32,
    /// Smoothed frame time in milliseconds.
    pub frame_ms: f32,
    /// Wall-clock cost of the avian physics step, ms. `None` when no
    /// physics-aware plugin is publishing.
    pub physics_ms: Option<f32>,
}

/// Flip the perf HUD on/off. Persisted via `lunco-settings`.
#[Command(default)]
pub struct TogglePerfHud {
    /// `true` enables the HUD; `false` hides it.
    pub enabled: bool,
}

#[on_command(TogglePerfHud)]
fn on_toggle_perf_hud(trigger: On<TogglePerfHud>, mut settings: ResMut<PerfHudSettings>) {
    let new = trigger.event().enabled;
    if settings.enabled != new {
        settings.enabled = new;
    }
}

register_commands!(on_toggle_perf_hud,);

/// Read smoothed FPS / frame time from the diagnostics store into
/// [`PerfStats`]. Bails when the HUD is disabled.
fn sample_frame_time(
    diags: Res<DiagnosticsStore>,
    settings: Res<PerfHudSettings>,
    mut stats: ResMut<PerfStats>,
) {
    if !settings.enabled {
        if stats.fps != 0.0 || stats.frame_ms != 0.0 || stats.physics_ms.is_some() {
            *stats = PerfStats::default();
        }
        return;
    }
    if let Some(d) = diags.get(&FrameTimeDiagnosticsPlugin::FPS) {
        if let Some(v) = d.smoothed() {
            stats.fps = v as f32;
        }
    }
    if let Some(d) = diags.get(&FrameTimeDiagnosticsPlugin::FRAME_TIME) {
        if let Some(v) = d.smoothed() {
            stats.frame_ms = v as f32;
        }
    }
}

/// Push the perf HUD's row into the workbench Settings menu.
fn register_settings_menu(world: &mut World) {
    use bevy_egui::egui;
    let Some(mut layout) = world.get_resource_mut::<crate::WorkbenchLayout>() else {
        return;
    };
    layout.register_settings(|ui, world| {
        ui.label(egui::RichText::new("Performance HUD").weak().small());
        let mut settings = world.resource_mut::<PerfHudSettings>();
        ui.checkbox(&mut settings.enabled, "Show FPS / frame time in status bar")
            .on_hover_text(
                "Bottom-right of the status bar shows live FPS, frame \
                 time, and physics step time when an avian-aware crate \
                 is loaded. Persisted to ~/.lunco/settings.json.",
            );
    });
}

/// Adds [`PerfStats`] (live samples), [`PerfHudSettings`] (persisted
/// pref via `lunco-settings`), the [`TogglePerfHud`] command, Bevy's
/// frame-time diagnostics, and the Settings-menu row. Idempotent.
pub struct PerfHudPlugin;

impl Plugin for PerfHudPlugin {
    fn build(&self, app: &mut App) {
        app.register_settings_section::<PerfHudSettings>();
        app.init_resource::<PerfStats>();
        if !app.is_plugin_added::<FrameTimeDiagnosticsPlugin>() {
            app.add_plugins(FrameTimeDiagnosticsPlugin::default());
        }
        app.add_systems(Startup, register_settings_menu);
        app.add_systems(Update, sample_frame_time);
        register_all_commands(app);
    }
}
