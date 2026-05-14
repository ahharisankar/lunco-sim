//! Miscellaneous utility commands: Ping and Exit.

use bevy::prelude::*;
use lunco_core::{Command, on_command};

// ─── Command Structs ─────────────────────────────────────────────────────────

/// API readiness probe.
#[Command(default)]
pub struct Ping {}

/// Gracefully shut down the application.
#[Command(default)]
pub struct Exit {}

// ─── Observers ───────────────────────────────────────────────────────────────

#[on_command(Ping)]
pub fn on_ping(_cmd: Ping) {
    // Intentional no-op.
}

#[on_command(Exit)]
pub fn on_exit(_trigger: On<Exit>, mut commands: Commands) {
    bevy::log::info!("[Exit] requested — routing through app-close flow");
    commands.queue(|world: &mut World| {
        crate::ui::commands::lifecycle::request_app_close(world);
    });
}
