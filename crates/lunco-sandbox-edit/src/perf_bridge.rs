//! Bridge avian's `PhysicsTotalDiagnostics` into workbench's
//! `PerfStats.physics_ms`. Lives here (not in `lunco-workbench`)
//! because the workbench stays physics-agnostic — it just exposes
//! an `Option<f32>` field for any crate that knows about avian to
//! populate.

use avian3d::diagnostics::{
    PhysicsDiagnosticsPlugin, PhysicsTotalDiagnostics, PhysicsTotalDiagnosticsPlugin,
};
use bevy::prelude::*;
use lunco_workbench::perf_hud::{PerfHudSettings, PerfStats};

/// Adds avian's diagnostics plugins (the framework one + the
/// total-step one that actually inserts `PhysicsTotalDiagnostics`)
/// and a sampler that copies `step_time` into `PerfStats.physics_ms`
/// when the HUD is enabled.
///
/// `PhysicsTotalDiagnosticsPlugin` *does* show up as a per-step
/// spike source in profiling (~30 ms bursts every few seconds),
/// but it's only ~10% of avian's overall spike contribution and
/// gating it on the persisted HUD-at-startup flag confused users
/// who toggle the HUD at runtime ("phys reads zero"). Always-on is
/// the better tradeoff.
pub struct PerfBridgePlugin;

impl Plugin for PerfBridgePlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<PhysicsDiagnosticsPlugin>() {
            app.add_plugins(PhysicsDiagnosticsPlugin);
        }
        if !app.is_plugin_added::<PhysicsTotalDiagnosticsPlugin>() {
            app.add_plugins(PhysicsTotalDiagnosticsPlugin);
        }
        app.add_systems(Update, sample_physics_step);
    }
}

fn sample_physics_step(
    diags: Option<Res<PhysicsTotalDiagnostics>>,
    settings: Res<PerfHudSettings>,
    mut stats: ResMut<PerfStats>,
) {
    if !settings.enabled {
        if stats.physics_ms.is_some() {
            stats.physics_ms = None;
        }
        return;
    }
    let Some(d) = diags else {
        stats.physics_ms = None;
        return;
    };
    stats.physics_ms = Some(d.step_time.as_secs_f32() * 1000.0);
}
