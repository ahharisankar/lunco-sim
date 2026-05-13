//! # Ephemeris abstraction
//!
//! Defines the [`EphemerisProvider`] trait and the [`EphemerisResource`] that
//! systems in this crate query (missions, trajectories, body positioning).
//! No heavy planetary-theory dependencies live here — they're in the sibling
//! crate `lunco-celestial-ephemeris`, which provides
//! `CelestialEphemerisProvider` (VSOP2013 + ELP/MPP02 + JPL Horizons CSV)
//! and an `EphemerisPlugin` that drops it into `EphemerisResource`.
//!
//! Apps that don't add `lunco-celestial-ephemeris` get the [`NoOpEphemerisProvider`]
//! installed by [`crate::CelestialPlugin`]: bodies stay put (every position
//! returns zero). That's fine for the Modelica workbench and any sandbox
//! scene that places bodies explicitly; orbital sims add the heavy crate.

use bevy::prelude::*;
use bevy::math::DVec3;

use std::sync::Arc;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct CsvDataPoint {
    pub jd: f64,
    pub pos_au: DVec3,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MissionConfig {
    pub ephemeris_sources: Option<Vec<EphemerisSource>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EphemerisSource {
    pub target_id: i32,
    pub command: String,
    pub center: String,
    pub ref_plane: String,
    pub start_time: String,
    pub stop_time: String,
    pub step_size: String,
}

/// Abstract interface for any system providing spatial state over time.
pub trait EphemerisProvider: Send + Sync + 'static {
    /// Returns the position of a body relative to its parent (Ecliptic J2000, AU).
    fn position(&self, body_id: i32, epoch_jd: f64) -> DVec3;

    /// Returns the Heliocentric J2000 position of a body by recursively
    /// resolving the gravitational hierarchy.
    fn global_position(&self, body_id: i32, epoch_jd: f64) -> DVec3 {
        let mut pos = self.position(body_id, epoch_jd);
        let mut current_id = body_id;

        // Resolve position relative to Sun (NAIF 10) by walking up the parent tree.
        for _ in 0..10 {
            let parent_id = match current_id {
                399 => 3,     // Earth -> EMB
                301 => 3,     // Moon -> EMB
                3 => 10,      // EMB -> Sun
                -1024 => 399, // Custom Mission -> Earth
                10 => break,
                _ => break,
            };
            if parent_id == 10 {
                break;
            }
            pos += self.position(parent_id, epoch_jd);
            current_id = parent_id;
        }
        pos
    }
}

/// Thread-safe resource facilitating access to the active ephemeris engine.
#[derive(Resource)]
pub struct EphemerisResource {
    pub provider: Arc<dyn EphemerisProvider>,
}

/// Returns zero for every body at every epoch. Installed by default so
/// downstream systems that depend on `Res<EphemerisResource>` don't panic.
/// Apps that want real planetary positions add `lunco-celestial-ephemeris`
/// and its `EphemerisPlugin`, which overwrites the resource.
pub struct NoOpEphemerisProvider;

impl EphemerisProvider for NoOpEphemerisProvider {
    fn position(&self, _body_id: i32, _epoch_jd: f64) -> DVec3 {
        DVec3::ZERO
    }
}
