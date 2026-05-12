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
    bevy::log::info!("[Exit] AppExit triggered via API");
    commands.queue(|world: &mut World| {
        if let Some(mut messages) =
            world.get_resource_mut::<bevy::ecs::message::Messages<bevy::app::AppExit>>()
        {
            messages.write(bevy::app::AppExit::Success);
        }
    });
}
