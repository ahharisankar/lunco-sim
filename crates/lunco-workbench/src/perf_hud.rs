//! Performance HUD for the status bar.
//!
//! Off by default. Persisted to `~/.lunco/perf_hud.json` so the
//! user's choice survives restarts. Three ways to flip it:
//!
//! - **Settings menu** — `Settings ▸ Performance HUD` checkbox.
//! - **Typed command** — [`TogglePerfHud`] over the API/script bus.
//! - **Direct mutation** — write to `PerfStats` from any system.
//!
//! Workbench itself stays physics-agnostic: the `physics_ms` field is
//! a plain `Option<f32>` that another crate (e.g. `lunco-sandbox-edit`)
//! populates when avian is in the build.

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::prelude::*;
use lunco_core::{Command, on_command, register_commands};
use serde::{Deserialize, Serialize};

/// Live perf numbers + the user's HUD preference.
///
/// `enabled` is the persistent setting; the rest are sampled each
/// frame while it's on.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct PerfStats {
    /// Master toggle. Persisted via [`PerfHudSettings`] to disk so
    /// it survives restarts. When `false`, samplers bail and the
    /// status bar hides the perf segment.
    pub enabled: bool,
    /// Smoothed FPS from Bevy's `FrameTimeDiagnosticsPlugin`.
    pub fps: f32,
    /// Smoothed frame time in milliseconds.
    pub frame_ms: f32,
    /// Wall-clock cost of the avian physics step, ms. `None` when no
    /// physics-aware plugin is publishing.
    pub physics_ms: Option<f32>,
}

/// On-disk settings for the perf HUD. Mirror of the persistable
/// subset of [`PerfStats`]. Stored as `~/.lunco/perf_hud.json`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default)]
pub struct PerfHudSettings {
    /// Whether the HUD shows in the status bar.
    pub enabled: bool,
}

fn settings_path() -> std::path::PathBuf {
    lunco_assets::user_config_dir().join("perf_hud.json")
}

fn load_settings() -> PerfHudSettings {
    match std::fs::read_to_string(settings_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => PerfHudSettings::default(),
    }
}

fn save_settings(s: &PerfHudSettings) {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        if let Err(e) = std::fs::write(&path, json) {
            warn!("[PerfHud] save to {} failed: {e}", path.display());
        }
    }
}

/// Flip the perf HUD on/off. Persists the new value to disk.
#[Command(default)]
pub struct TogglePerfHud {
    /// `true` enables the HUD and starts sampling; `false` hides it.
    pub enabled: bool,
}

#[on_command(TogglePerfHud)]
fn on_toggle_perf_hud(
    trigger: On<TogglePerfHud>,
    mut stats: ResMut<PerfStats>,
    mut last: ResMut<PerfHudLastSaved>,
) {
    let new = trigger.event().enabled;
    if stats.enabled == new {
        return;
    }
    stats.enabled = new;
    if !new {
        stats.fps = 0.0;
        stats.frame_ms = 0.0;
        stats.physics_ms = None;
    }
    let s = PerfHudSettings { enabled: new };
    save_settings(&s);
    last.0 = s;
}

register_commands!(on_toggle_perf_hud,);

/// Last-saved snapshot for change detection — write to disk only
/// when the user actually flips the setting (not on every frame's
/// `PerfStats` mutation as samplers update fps / frame_ms).
#[derive(Resource, Default, Debug, Clone, Copy)]
struct PerfHudLastSaved(PerfHudSettings);

fn load_settings_at_startup(
    mut stats: ResMut<PerfStats>,
    mut last: ResMut<PerfHudLastSaved>,
) {
    let s = load_settings();
    stats.enabled = s.enabled;
    last.0 = s;
}

/// Catches direct mutations to `PerfStats.enabled` (Settings-menu
/// checkbox, scripted writes) and persists. The command observer
/// above already saves; this is the safety net for non-command
/// paths.
fn persist_on_change(
    stats: Res<PerfStats>,
    mut last: ResMut<PerfHudLastSaved>,
) {
    if !stats.is_changed() {
        return;
    }
    if last.0.enabled == stats.enabled {
        return;
    }
    let s = PerfHudSettings { enabled: stats.enabled };
    save_settings(&s);
    last.0 = s;
}

/// Read smoothed FPS / frame time from the diagnostics store into
/// [`PerfStats`]. Bails when the HUD is disabled.
fn sample_frame_time(diags: Res<DiagnosticsStore>, mut stats: ResMut<PerfStats>) {
    if !stats.enabled {
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
    let Some(mut layout) = world
        .get_resource_mut::<crate::WorkbenchLayout>()
    else {
        return;
    };
    layout.register_settings(|ui, world| {
        ui.label(egui::RichText::new("Performance HUD").weak().small());
        let mut stats = world.resource_mut::<PerfStats>();
        if ui
            .checkbox(&mut stats.enabled, "Show FPS / frame time in status bar")
            .on_hover_text(
                "Bottom-right of the status bar shows live FPS, frame \
                 time, and physics step time when an avian-aware crate \
                 is loaded. Persisted to ~/.lunco/perf_hud.json.",
            )
            .changed()
        {
            // Direct mutation path — `persist_on_change` will catch
            // this next frame and write to disk.
        }
    });
}

/// Adds [`PerfStats`], the [`TogglePerfHud`] command, Bevy's
/// frame-time diagnostics, disk persistence, and the Settings-menu
/// row. Idempotent.
pub struct PerfHudPlugin;

impl Plugin for PerfHudPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PerfStats>();
        app.init_resource::<PerfHudLastSaved>();
        if !app.is_plugin_added::<FrameTimeDiagnosticsPlugin>() {
            app.add_plugins(FrameTimeDiagnosticsPlugin::default());
        }
        app.add_systems(Startup, load_settings_at_startup);
        app.add_systems(Startup, register_settings_menu);
        app.add_systems(Update, (sample_frame_time, persist_on_change));
        register_all_commands(app);
    }
}
