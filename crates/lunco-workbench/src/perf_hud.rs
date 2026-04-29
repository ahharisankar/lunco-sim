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

/// How many frame-time samples to keep for the status-bar sparkline.
/// At 60 FPS that's about 4 seconds — long enough to spot a hitch
/// in your peripheral vision, short enough that the plot redraws
/// quickly when conditions change.
pub const FRAME_HISTORY_LEN: usize = 240;

/// FixedUpdate per-frame metrics. Bevy's `FixedUpdate` schedule runs
/// 0, 1, 2, … times per frame depending on accumulated time. When a
/// slow frame falls behind, the next frame catches up by running
/// extra ticks — which makes the next frame slow too. That cascade
/// is the prime suspect for the cyclic spikes.
///
/// We track tick count + total fixed-update wall-time per frame so
/// the spike logger can correlate them with `frame_ms`.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct FixedUpdateMetrics {
    /// How many times `FixedUpdate` ran during the last frame.
    pub ticks_last_frame: u32,
    /// Wall-clock time, in ms, spent inside FixedUpdate runs that
    /// occurred during the last frame (sum across all ticks).
    pub fixed_ms_last_frame: f32,
    /// Working accumulators reset each Update frame.
    ticks_acc: u32,
    ms_acc: f32,
}

/// Live, per-frame perf samples. Not persisted — these are reset
/// when the HUD is disabled and resampled while it's on.
#[derive(Resource, Default, Debug, Clone)]
pub struct PerfStats {
    /// Smoothed FPS from Bevy's `FrameTimeDiagnosticsPlugin`.
    pub fps: f32,
    /// Smoothed frame time in milliseconds.
    pub frame_ms: f32,
    /// Wall-clock cost of the avian physics step, ms. `None` when no
    /// physics-aware plugin is publishing.
    pub physics_ms: Option<f32>,
    /// Ring buffer of recent frame times, oldest first. Used by the
    /// status-bar sparkline so spikes that the smoothed `frame_ms`
    /// number hides become visible. Capped at [`FRAME_HISTORY_LEN`].
    pub frame_history: std::collections::VecDeque<f32>,
}

impl PerfStats {
    /// `(min, max, p99)` over `frame_history`, all in ms. Returns
    /// `None` when the history is empty so callers can skip drawing.
    pub fn frame_ms_stats(&self) -> Option<(f32, f32, f32)> {
        if self.frame_history.is_empty() {
            return None;
        }
        let mut sorted: Vec<f32> = self.frame_history.iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let min = *sorted.first().unwrap();
        let max = *sorted.last().unwrap();
        let p99_idx = ((sorted.len() as f32) * 0.99) as usize;
        let p99 = sorted[p99_idx.min(sorted.len() - 1)];
        Some((min, max, p99))
    }
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
        if stats.fps != 0.0 || stats.frame_ms != 0.0 || stats.physics_ms.is_some()
            || !stats.frame_history.is_empty()
        {
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
        // Push the **raw** (un-smoothed) value to the history so
        // spikes survive into the sparkline; keep the smoothed value
        // for the headline number where stability is preferred.
        if let Some(raw) = d.value() {
            if stats.frame_history.len() == FRAME_HISTORY_LEN {
                stats.frame_history.pop_front();
            }
            stats.frame_history.push_back(raw as f32);
        }
        if let Some(v) = d.smoothed() {
            stats.frame_ms = v as f32;
        }
    }
}

/// Logs a line whenever a frame exceeds the spike threshold. Includes
/// FixedUpdate metrics so we can see whether the spike correlates
/// with avian/cosim catchup ticks (the "1 slow frame breeds the next"
/// cascade hypothesis).
const SPIKE_THRESHOLD_MS: f32 = 20.0;

fn log_frame_spikes(
    settings: Res<PerfHudSettings>,
    stats: Res<PerfStats>,
    fixed_metrics: Res<FixedUpdateMetrics>,
    phases: Res<PhaseTimers>,
    windows: Query<&bevy::window::Window>,
    // (frame_index, last_spike_frame)
    mut state: Local<(u64, Option<u64>)>,
) {
    if !settings.enabled {
        return;
    }
    state.0 += 1;
    let Some(&latest_ms) = stats.frame_history.back() else { return };
    if latest_ms < SPIKE_THRESHOLD_MS {
        return;
    }
    // Skip the structural reactive-low-power tick — when no window has
    // focus, `WinitSettings.unfocused_mode` parks the loop for ~1s, which
    // shows up here as a 1000ms "frame". That's intentional idle, not a
    // hitch worth logging.
    let any_focused = windows.iter().any(|w| w.focused);
    if !any_focused && latest_ms > 200.0 {
        return;
    }
    let frame = state.0;
    let since = state.1.map(|prev| frame - prev);
    state.1 = Some(frame);
    let ticks = fixed_metrics.ticks_last_frame;
    let fixed_ms = fixed_metrics.fixed_ms_last_frame;
    let main = phases.main_ms;
    let render_etc = (latest_ms - main).max(0.0);
    match since {
        Some(dt) => info!(
            "[spike] frame {} took {:.1}ms (main {:.1} [fixed {:.1}/{}t] + render+other {:.1}; +{}f)",
            frame, latest_ms, main, fixed_ms, ticks, render_etc, dt
        ),
        None => info!(
            "[spike] frame {} took {:.1}ms (main {:.1} [fixed {:.1}/{}t] + render+other {:.1}; first)",
            frame, latest_ms, main, fixed_ms, ticks, render_etc
        ),
    }
}

/// FixedFirst → FixedLast bracketing needs to share state between
/// two systems, which means a `Resource` (Locals are per-system).
#[derive(Resource, Default)]
struct FixedTickStart(Option<std::time::Instant>);

/// Total main-schedule (First..Last) wall time per frame. Spike
/// time minus this = render + extract + present time. Splits a
/// frame spike into "main thread CPU work" vs "render pipeline /
/// GPU sync". One bracket pair, no ordering tricks needed.
#[derive(Resource, Default)]
struct PhaseTimers {
    main_start: Option<std::time::Instant>,
    main_ms: f32,
}

fn main_start_marker(mut t: ResMut<PhaseTimers>) {
    t.main_start = Some(std::time::Instant::now());
}

fn main_end_marker(mut t: ResMut<PhaseTimers>) {
    if let Some(s) = t.main_start.take() {
        t.main_ms = s.elapsed().as_secs_f32() * 1000.0;
    }
}

/// FixedFirst: capture tick start.
fn fixed_tick_start(mut start: ResMut<FixedTickStart>) {
    start.0 = Some(std::time::Instant::now());
}

/// FixedLast: capture tick duration, accumulate into metrics.
fn fixed_tick_end(start: Res<FixedTickStart>, mut metrics: ResMut<FixedUpdateMetrics>) {
    let Some(t0) = start.0 else { return };
    let elapsed = t0.elapsed().as_secs_f32() * 1000.0;
    metrics.ticks_acc += 1;
    metrics.ms_acc += elapsed;
}

/// Last (main schedule, end of frame): publish accumulated metrics
/// then reset for the next frame.
fn publish_fixed_metrics(mut metrics: ResMut<FixedUpdateMetrics>) {
    metrics.ticks_last_frame = metrics.ticks_acc;
    metrics.fixed_ms_last_frame = metrics.ms_acc;
    metrics.ticks_acc = 0;
    metrics.ms_acc = 0.0;
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
///
/// `FrameTimeDiagnosticsPlugin` and the per-frame samplers are only
/// registered when the HUD is enabled in the persisted settings —
/// they have non-trivial cost (smoothing buffers, change-tick
/// allocations, command flushes around diagnostic registration) and
/// the spike profile showed them as a meaningful contributor when
/// the user wasn't even looking at the HUD. Toggling the HUD on at
/// runtime requires a restart to pick up the diagnostic plugin; this
/// matches the typical flow ("turn it on while debugging perf, then
/// off and forget").
pub struct PerfHudPlugin;

impl Plugin for PerfHudPlugin {
    fn build(&self, app: &mut App) {
        app.register_settings_section::<PerfHudSettings>();
        app.init_resource::<PerfStats>();
        app.init_resource::<FixedUpdateMetrics>();
        app.init_resource::<FixedTickStart>();
        app.init_resource::<PhaseTimers>();
        // FrameTime diagnostics + frame sampler + spike logger are
        // always registered — they're cheap (a few µs/frame), and
        // toggling the HUD at runtime needs the data to be there
        // already. Each system bails early when the HUD is off.
        if !app.is_plugin_added::<FrameTimeDiagnosticsPlugin>() {
            app.add_plugins(FrameTimeDiagnosticsPlugin::default());
        }
        app.add_systems(Update, sample_frame_time);
        app.add_systems(Update, log_frame_spikes.after(sample_frame_time));
        app.add_systems(bevy::app::FixedFirst, fixed_tick_start);
        app.add_systems(bevy::app::FixedLast, fixed_tick_end);
        app.add_systems(Last, publish_fixed_metrics);
        // Phase brackets: total main-schedule wall time per frame.
        // Spike time minus this = render-pipeline / GPU sync /
        // pipelined-rendering wait. One bracket; no schedule
        // ordering tricks.
        app.add_systems(First, main_start_marker);
        app.add_systems(Last, main_end_marker.before(publish_fixed_metrics));
        app.add_systems(Startup, register_settings_menu);
        register_all_commands(app);
    }
}

