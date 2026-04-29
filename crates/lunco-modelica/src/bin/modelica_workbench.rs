//! Generic engineering workbench for testing any Modelica model.

use bevy::prelude::*;
use bevy_egui::EguiPlugin;
use lunco_modelica::ModelicaPlugin;

fn main() {
    // Cap rayon's global pool to leave headroom for Bevy's renderer.
    //
    // History: when projection + ast_refresh still ran on rayon, the
    // unconfigured pool grabbed `num_cpus - 1` threads and starved
    // the renderer's pipelined extract — every Add/Move edit froze
    // the UI for 1.5–2.5 s. Hard cap at 2 fixed it.
    //
    // After the SyntaxCache refactor (commits TBD), projection +
    // ast_refresh both run on Bevy's `AsyncComputeTaskPool`, NOT on
    // rayon. The only remaining rayon caller is rumoca's
    // `parse_files_parallel`, which fires once at compile-time MSL
    // preload and again per file load — short bursts, not background
    // work that races the renderer. A cap of 2 there made first-
    // compile MSL preload 8× slower than CLI (~64 s vs 8 s wall;
    // worse under contention).
    //
    // New policy: leave 2 cores for Bevy (renderer + main), give the
    // rest to rumoca. On a 16-core machine that's 14 threads — close
    // to CLI parity. On low-core machines (≤4) we still cap at 2
    // because the original starvation problem dominates there.
    let n_cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let rayon_threads = if n_cpus <= 4 { 2 } else { n_cpus.saturating_sub(2) };
    let rayon_init = rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_threads)
        .build_global();
    match rayon_init {
        Ok(()) => eprintln!(
            "[modelica_workbench] rayon global pool capped at {rayon_threads} threads (of {n_cpus} CPUs)"
        ),
        Err(e) => eprintln!(
            "[modelica_workbench] WARN: rayon already initialised, our cap LOST: {e}"
        ),
    }

    // Mirror `LunCoApiConfig::from_args` so the title can advertise
    // the listening port — automation drives the workbench via this
    // port, having it visible in the title bar avoids confusion when
    // multiple instances run side-by-side (e.g. user on 3000 + a
    // sandboxed test on 3001).
    let api_port: Option<u16> = {
        let args: Vec<String> = std::env::args().collect();
        let mut port = None;
        for i in 0..args.len() {
            if args[i] == "--api" {
                port = Some(3000);
                if i + 1 < args.len() {
                    if let Ok(p) = args[i + 1].parse::<u16>() {
                        port = Some(p);
                    }
                }
                break;
            }
        }
        port
    };
    let window_title = match api_port {
        Some(p) => format!("LunCo Modelica Workbench — Listening on {p}"),
        None => "LunCo Modelica Workbench".to_string(),
    };

    let mut app = App::new();
    // Custom title bar: hide OS chrome (Linux/Windows) or merge with
    // content (macOS) so the egui menu bar doubles as the title bar —
    // same idea as Antigravity / VS Code's CSD. Drag + min/max/close
    // are wired up in `lunco-workbench`'s menu bar renderer.
    let primary_window = Window {
        title: window_title,
        #[cfg(not(target_os = "macos"))]
        decorations: false,
        #[cfg(target_os = "macos")]
        titlebar_transparent: true,
        #[cfg(target_os = "macos")]
        titlebar_show_title: false,
        #[cfg(target_os = "macos")]
        fullsize_content_view: true,
        ..default()
    };
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(primary_window),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        // Vello-backed diagram canvas — TBD.
        //
        // The pipeline (lunco-canvas's DiagramRenderer trait,
        // EguiRenderer + VelloRenderer backends, per-tab offscreen
        // render targets in `lunco_modelica::ui::vello_canvas`) is
        // landed and renders all MSL geometry primitives. Re-enable
        // by un-commenting the two `add_plugins` lines below once
        // the text-rendering issue (bevy_vello 0.13.1 entities
        // don't appear in offscreen `RenderTarget::Image`) is
        // resolved upstream or worked around. The egui canvas
        // remains the production paint path until then.
        // .add_plugins(bevy_vello::VelloPlugin::default())
        // .add_plugins(lunco_modelica::ui::vello_canvas::VelloCanvasPlugin)
        .add_plugins(lunco_workbench::WorkbenchPlugin)
        // LuncoVizPlugin must come before any plugin that publishes
        // signals (ModelicaPlugin below) so the `SignalRegistry`
        // resource is present when the worker starts mirroring
        // samples into it.
        .add_plugins(lunco_viz::LuncoVizPlugin)
        .add_plugins(ModelicaPlugin)
        .add_plugins(lunco_modelica::msl_remote::MslRemotePlugin)
        .add_systems(Startup, setup_sandbox);

    #[cfg(feature = "lunco-api")]
    app.add_plugins(lunco_api::LunCoApiPlugin::default());

    // Force continuous frame rate even when the window is unfocused
    // so the HTTP API stays responsive under automation. Default
    // winit throttles unfocused windows; the bridge-drain system
    // only runs on ticks, so a throttled window masquerades as an
    // "app hang" when driving the workbench from curl.
    use bevy::winit::{UpdateMode, WinitSettings};
    app.insert_resource(WinitSettings {
        focused_mode: UpdateMode::Continuous,
        unfocused_mode: UpdateMode::Continuous,
    });

    // Physics fixed timestep: 60 Hz. Modelica stepping runs in
    // FixedUpdate so the worker receives a predictable per-tick dt.
    // Matches the Avian / lunco-cosim convention; the worker hands
    // `time.delta_secs_f64()` straight to `stepper.step()`.
    app.insert_resource(Time::<Fixed>::from_hz(60.0));

    app.run();
}

fn setup_sandbox(mut commands: Commands) {
    // Start empty: the user lands on the Welcome tab, opens whatever
    // they need via Package Browser / Twin / Ctrl+N. Auto-loading
    // Battery was a debug convenience that confused new users —
    // `cargo run` would show a random model with no explanation.
    //
    // `PrimaryEguiContext` pins egui's window-side rendering to *this*
    // camera. Without the explicit marker, adding a second `Camera2d`
    // (e.g. the vello-spike's offscreen camera) makes bevy_egui's
    // auto-context-pick ambiguous and the workbench chrome silently
    // stops rendering to the window — only the offscreen vello content
    // shows up in screenshots.
    commands.spawn((Camera2d, bevy_egui::PrimaryEguiContext));
}

// Phase-0 spike test scene removed; Phase 1 lives in
// `lunco_modelica::ui::vello_canvas`.
