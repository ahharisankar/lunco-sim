//! Headless tests for cross-truth policy chokepoints
//! (`docs/architecture/B0_CROSS_TRUTH_POLICY.md`).
//!
//! Each rule's *helper-level* contract pins here; observer wiring is
//! exercised in single-file harness tests where Bevy is required.

use lunco_modelica::ui::wasm_autosave::{IsGestureActive, should_autosave};

// ─────────────────────────────────────────────────────────────────────
// R1 — autosave gates on active gesture
// ─────────────────────────────────────────────────────────────────────

#[test]
fn r1_should_autosave_writes_for_clean_untitled() {
    assert!(should_autosave(false, true), "untitled + idle = write");
}

#[test]
fn r1_should_autosave_skips_file_backed() {
    // File-backed docs have a real save path; localStorage write
    // would shadow it.
    assert!(!should_autosave(false, false), "file-backed doc never autosaves");
}

#[test]
fn r1_should_autosave_skips_during_gesture() {
    // Mid-drag / mid-edit: a snapshot now would capture transient
    // state ("one component in two places") and persist it.
    assert!(!should_autosave(true, true), "active gesture blocks autosave");
}

#[test]
fn r1_active_gesture_or_file_backed_blocks() {
    // Both filters required; either one blocking is enough.
    assert!(!should_autosave(true, false), "gesture AND file-backed = no");
}

#[test]
fn r1_is_gesture_active_default_is_idle() {
    let g = IsGestureActive::default();
    assert!(!g.any(), "default is all-clear");
    assert!(!g.canvas);
    assert!(!g.text);
    assert!(!g.modal);
}

#[test]
fn r1_is_gesture_active_any_is_or_of_sources() {
    // Each source independently activates the gate.
    let mut g = IsGestureActive::default();
    g.canvas = true;
    assert!(g.any());

    let mut g = IsGestureActive::default();
    g.text = true;
    assert!(g.any());

    let mut g = IsGestureActive::default();
    g.modal = true;
    assert!(g.any());
}

#[test]
fn r1_is_gesture_active_independent_sources() {
    // Two sources active at once; clearing one alone doesn't open
    // the gate. Pins the regression class where canvas-release
    // would re-enable autosave while a modal is still open.
    let mut g = IsGestureActive::default();
    g.canvas = true;
    g.modal = true;
    assert!(g.any());
    g.canvas = false;
    assert!(g.any(), "modal still active — gate stays closed");
    g.modal = false;
    assert!(!g.any());
}
