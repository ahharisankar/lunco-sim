//! Utility helpers and shared caches for the Diagram Panel.

use bevy::prelude::*;
use crate::annotations::Icon;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Wall-clock timestamp of the most recent `apply_ops` call. Used
/// by the post-Add window tracker in the panel render to log every
/// frame for ~2 seconds after each Add.
pub(crate) static LAST_APPLY_AT: Mutex<Option<web_time::Instant>> = Mutex::new(None);

/// Process-shared cache of resolved port-connector icons.
/// Memoises `qualified_class → Option<Icon>` across all frames.
pub(crate) fn port_icon_cache() -> &'static Mutex<HashMap<String, Option<Icon>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Icon>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clear the process-shared port-icon cache.
pub(crate) fn invalidate_port_icon_cache() {
    port_icon_cache().lock().expect("port_icon_cache poisoned").clear();
}

/// Mark a phase in the render loop for tracing.
pub(crate) fn mark(label: &'static str, t: &mut web_time::Instant, log: &mut Vec<(&'static str, f64)>) {
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    if ms > 1.0 {
        log.push((label, ms));
    }
    *t = web_time::Instant::now();
}

pub(crate) fn log_frame_times(total_ms: f64, render_canvas_ms: f64) {
    let mut force_log = false;
    if let Ok(guard) = LAST_APPLY_AT.lock() {
        if let Some(t) = *guard {
            if t.elapsed().as_secs_f64() < 2.0 {
                force_log = true;
            }
        }
    }

    if force_log {
        bevy::log::debug!(
            "[CanvasDiagram] frame: total={total_ms:.1}ms render_canvas={render_canvas_ms:.1}ms (post-apply window)"
        );
    } else if total_ms > 16.0 {
        bevy::log::warn!(
            "[CanvasDiagram] slow frame: total={total_ms:.1}ms render_canvas={render_canvas_ms:.1}ms"
        );
    }
}
