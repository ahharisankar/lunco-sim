//! Off-thread Modelica simulation worker + Bevy bridge.
//!
//! `modelica_worker` runs on its own OS thread (it owns a
//! `!Send` `SimStepper`, so it can't live on the Bevy main loop). The
//! Bevy systems `spawn_modelica_requests` and
//! `handle_modelica_responses` exchange `ModelicaCommand` /
//! `ModelicaResult` messages with it via crossbeam channels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use rumoca_sim::{SimStepper, StepperOptions};

use lunco_assets::modelica_dir;

use crate::ast_extract::strip_input_defaults;
use crate::sim_stream::{SimSnapshot, SimStream};
use crate::ui::commands::CompileModel;
use crate::{ModelicaCompiler, SimStreamRegistry};

/// Channels for communicating with the background simulation worker.
///
/// This resource holds the crossbeam channel endpoints that the main Bevy thread
/// uses to send commands to and receive results from the `modelica_worker` thread.
#[derive(Resource)]
pub struct ModelicaChannels {
    /// Sender for `ModelicaCommand` -> worker
    pub tx: Sender<ModelicaCommand>,
    /// Receiver for `ModelicaResult` <- worker
    pub rx: Receiver<ModelicaResult>,
    /// Receiver for `ModelicaCommand` <- UI (used by wasm32 inline worker)
    #[cfg(target_arch = "wasm32")]
    pub rx_cmd: Receiver<ModelicaCommand>,
    /// Sender for `ModelicaResult` -> UI (used by wasm32 inline worker)
    #[cfg(target_arch = "wasm32")]
    pub tx_res: Sender<ModelicaResult>,
}

/// Commands sent to the background simulation worker.
///
/// Each command targets a specific Bevy `Entity` and carries a `session_id` for
/// fencing stale results. The worker owns all `SimStepper` instances, keyed by entity.
pub enum ModelicaCommand {
    /// Advance simulation by one timestep. Sent every frame from `spawn_modelica_requests`.
    Step {
        entity: Entity,
        session_id: u64,
        model_path: PathBuf,
        model_name: String,
        inputs: Vec<(String, f64)>,
        dt: f64,
    },
    /// Compile Modelica source code into a DAE and create a new SimStepper.
    ///
    /// The compiled DAE is cached per entity for instant Reset and fast stepper rebuilds.
    Compile {
        entity: Entity,
        session_id: u64,
        model_name: String,
        source: String,
        /// Sources from other open Modelica documents, as
        /// `(filename, source)` pairs. Loaded into the rumoca
        /// session before the primary `source` so cross-doc class
        /// references (e.g. an untitled `RocketStage` referencing
        /// `AnnotatedRocketStage.Tank` from a sibling untitled
        /// package) resolve. Empty when only one doc is open.
        extra_sources: Vec<(String, String)>,
        /// Lock-free snapshot handle the worker publishes into after
        /// every successful Step (Phase A of the multi-sim arch).
        /// `None` = legacy path; main thread still receives per-sample
        /// data via `ModelicaResult.outputs` and pushes it into
        /// `SignalRegistry`. When `Some`, the worker updates the
        /// stream directly and the main-thread handler can skip the
        /// per-sample push loop.
        stream: Option<SimStream>,
    },
    /// Update parameter values by recompiling with modified source code.
    ///
    /// Since Modelica parameters are compile-time constants, changing them requires
    /// recompilation. This command takes the full source with substituted parameter values,
    /// creates a new stepper, and updates the cached DAE.
    UpdateParameters {
        entity: Entity,
        session_id: u64,
        model_name: String,
        source: String,
    },
    /// Reset the stepper to initial conditions using the cached DAE (instant, no recompilation).
    Reset {
        entity: Entity,
        session_id: u64,
    },
    /// Remove the stepper and cached DAE (entity despawned).
    Despawn {
        entity: Entity,
    }
}

use std::sync::Arc;

/// Results received from the background simulation worker.
///
/// Contains simulation outputs, detected symbols, and error information.
/// The `session_id` field is used by `handle_modelica_responses` to fence stale results.
pub struct ModelicaResult {
    pub entity: Entity,
    pub session_id: u64,
    pub new_time: f64,
    pub outputs: Vec<(String, f64)>,
    pub detected_symbols: Vec<(String, f64)>,
    pub error: Option<String>,
    pub log_message: Option<String>,
    pub is_new_model: bool,
    pub is_parameter_update: bool,
    pub is_reset: bool,
    /// Input variable names discovered from the model (input Real ...).
    /// These can be changed at runtime without recompilation.
    pub detected_input_names: Vec<String>,
    /// Per-variable Modelica description strings (the `"..."` comment after
    /// a declaration — MLS §A.2.5 `description-string`). Collected from
    /// the compiled DAE on `is_new_model` / `is_parameter_update` so the
    /// UI can show them as hover tooltips. Only populated on compile-type
    /// results; Step results leave this empty.
    pub detected_descriptions: Vec<(String, String)>,
}

impl Default for ModelicaResult {
    fn default() -> Self {
        Self {
            entity: Entity::PLACEHOLDER,
            session_id: 0,
            new_time: 0.0,
            outputs: Vec::new(),
            detected_symbols: Vec::new(),
            error: None,
            log_message: None,
            is_new_model: false,
            is_parameter_update: false,
            is_reset: false,
            detected_input_names: Vec::new(),
            detected_descriptions: Vec::new(),
        }
    }
}

/// Cached compilation result per entity.
///
/// Stores the DAE and source hash so we can instantly rebuild a SimStepper
/// after Reset without recompiling, and detect when the Step command's
/// model_path points to stale source.
struct CachedModel {
    #[allow(dead_code)]
    session_id: u64,
    model_name: String,
    #[allow(dead_code)]
    source: Arc<str>,
    #[allow(dead_code)]
    dae: Box<rumoca_session::compile::DaeCompilationResult>,
}

/// Collect every readable variable from the stepper — states, inputs, and
/// (on rumoca `main`) algebraic / output reconstructions via
/// `EliminationResult`. Non-finite values are dropped so the UI never
/// plots NaN. Filtering out parameters / inputs happens downstream in
/// [`handle_modelica_responses`]; we report everything here so the UI has
/// the full picture and decides what goes into `model.variables`.
fn collect_stepper_observables(stepper: &SimStepper) -> Vec<(String, f64)> {
    stepper
        .state()
        .values
        .into_iter()
        .filter(|(name, val)| val.is_finite() && name != "time")
        .collect()
}

/// Helper to build a ModelicaResult with defaults.
fn result_ok(entity: Entity, session_id: u64) -> ModelicaResult {
    ModelicaResult {
        entity,
        session_id,
        new_time: 0.0,
        outputs: Vec::new(),
        detected_symbols: Vec::new(),
        error: None, log_message: None, is_new_model: false,
        is_parameter_update: false, is_reset: false,
        detected_input_names: Vec::new(),
        detected_descriptions: Vec::new(),
    }
}

/// Pull every variable's Modelica description string (`"..."` after a
/// declaration, per MLS §A.2.5) straight from the source AST.
///
/// Rumoca's DAE drops component descriptions during compile → DAE
/// lowering (as of the rumoca commit pinned in `Cargo.lock` — the
/// field `Dae::Variable.description` is always `None` in practice),
/// so we re-parse the source AST instead. Cheap enough for compile /
/// parameter-update events (rumoca parse is fast and cached).
fn collect_variable_descriptions(source: &str) -> Vec<(String, String)> {
    crate::ast_extract::extract_descriptions(source)
        .into_iter()
        .collect()
}

/// The background worker that owns the !Send SimSteppers and cached DAEs.
pub fn modelica_worker(rx: Receiver<ModelicaCommand>, tx: Sender<ModelicaResult>) {
    let mut steppers: HashMap<Entity, (u64, String, SimStepper)> = HashMap::default();
    let mut current_sessions: HashMap<Entity, u64> = HashMap::default();
    // DAE cache per entity — enables instant Reset and fast Step auto-init
    let mut cached_models: HashMap<Entity, CachedModel> = HashMap::default();
    // Lock-free publish stream per entity (Phase A of the multi-sim
    // refactor — see `sim_stream.rs`). The UI side holds a clone of
    // the same `Arc<ArcSwap<SimSnapshot>>`; every successful Step
    // publishes a new snapshot so plots render without locking or
    // involving the main thread in per-sample work.
    let mut sim_streams: HashMap<Entity, SimStream> = HashMap::default();
    // Lazy compiler construction. `ModelicaCompiler::new` is now
    // cheap — it creates an empty session with no MSL loaded.
    // Actual MSL files are pulled into the session on demand by
    // `compile_str` based on what each compile's reachable closure
    // references. No reason to pre-build it.
    let mut compiler: Option<ModelicaCompiler> = None;

    while let Ok(first_cmd) = rx.recv() {
        let mut pending = vec![first_cmd];
        while let Ok(cmd) = rx.try_recv() { pending.push(cmd); }

        let mut to_process = Vec::new();
        for cmd in pending {
            if let Some(last) = to_process.last_mut() {
                if is_squashable(last, &cmd) {
                    if cmd_session(last) == cmd_session(&cmd) {
                        let _ = tx.send(result_ok(cmd_entity(last), cmd_session(last)));
                        *last = cmd;
                        continue;
                    }
                }
            }
            to_process.push(cmd);
        }

        for cmd in to_process {
            let tx_inner = tx.clone();
            // Instrumentation for the "sometimes stuck" class of bugs:
            // when the worker hangs (usually inside a pathological
            // rumoca compile on a malformed model), the main-thread
            // UI sees no progress and no log breadcrumb. These bracket
            // logs let us see exactly which command + model was
            // in-flight and how long it actually took, so a stall is
            // visible in `RUST_LOG=info` output instead of silent.
            let cmd_label = command_label(&cmd);
            let cmd_started = web_time::Instant::now();
            // `Step` fires at simulation rate (~60 Hz) — log at debug to
            // avoid drowning the console. One-shot commands (Compile,
            // Reset, …) stay at info because they're rare and useful.
            let is_hot_path = matches!(cmd, ModelicaCommand::Step { .. });
            if is_hot_path {
                log::debug!("[worker] begin: {}", cmd_label);
            } else {
                log::info!("[worker] begin: {}", cmd_label);
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match cmd {
                    ModelicaCommand::Reset { entity, session_id } => {
                        current_sessions.insert(entity, session_id);

                        if let Some(cached) = cached_models.get(&entity) {
                            // Strip input defaults from cached source and set them via set_input
                            let (stripped_source, input_defaults) = strip_input_defaults(&cached.source);

                            let mut opts = StepperOptions::default();
                            opts.atol = 1e-1; opts.rtol = 1e-1;
                            // Recompile stripped source to get a fresh stepper with input slots
                            let compiler = compiler.get_or_insert_with(ModelicaCompiler::new);
                            match compiler.compile_str(&cached.model_name, &stripped_source, "model.mo") {
                                Ok(comp_res) => {
                                    match SimStepper::new(&comp_res.dae, opts) {
                                        Ok(mut stepper) => {
                                            for (name, val) in &input_defaults {
                                                let _ = stepper.set_input(name, *val);
                                            }
                                            let input_names: Vec<String> = stepper.input_names().to_vec();
                                            let symbols = collect_stepper_observables(&stepper);
                                            let descriptions = collect_variable_descriptions(&stripped_source);
                                            steppers.insert(entity, (session_id, cached.model_name.clone(), stepper));
                                            let _ = tx_inner.send(ModelicaResult {
                                                entity, session_id, new_time: 0.0,
                                                outputs: Vec::new(),
                                                detected_symbols: symbols, error: None,
                                                log_message: Some("Reset complete.".to_string()),
                                                is_new_model: false, is_parameter_update: false, is_reset: true,
                                                detected_input_names: input_names,
                                                detected_descriptions: descriptions,
                                            });
                                        }
                                        Err(e) => {
                                            let mut r = result_ok(entity, session_id);
                                            r.error = Some(format!("Stepper Init Error: {:?}", e));
                                            r.is_reset = true;
                                            let _ = tx_inner.send(r);
                                        }
                                    }
                                }
                                Err(e) => {
                                    let mut r = result_ok(entity, session_id);
                                    r.error = Some(format!("Reset compile error: {:?}", e));
                                    r.is_reset = true;
                                    let _ = tx_inner.send(r);
                                }
                            }
                        } else {
                            steppers.remove(&entity);
                            let mut r = result_ok(entity, session_id);
                            r.is_reset = true;
                            r.log_message = Some("Reset complete (no cached model).".to_string());
                            let _ = tx_inner.send(r);
                        }
                    }
                    ModelicaCommand::UpdateParameters { entity, session_id, model_name, source } => {
                        if session_id < *current_sessions.get(&entity).unwrap_or(&0) {
                            let _ = tx_inner.send(result_ok(entity, session_id));
                            return;
                        }
                        current_sessions.insert(entity, session_id);

                        let temp_dir = modelica_dir().join(format!("{}_{}", entity.index(), entity.generation()));
                        let _ = std::fs::create_dir_all(&temp_dir);
                        let temp_path = temp_dir.join("model.mo");
                        if let Err(e) = std::fs::write(&temp_path, &source) {
                            let mut r = result_ok(entity, session_id);
                            r.error = Some(format!("IO Error: {:?}", e));
                            let _ = tx_inner.send(r);
                            return;
                        }

                        // Strip input defaults so they become real runtime slots
                        let (stripped_source, input_defaults) = strip_input_defaults(&source);

                        let compiler = compiler.get_or_insert_with(ModelicaCompiler::new);
                        match compiler.compile_str(&model_name, &stripped_source, "model.mo") {
                            Ok(comp_res) => {
                                let mut opts = StepperOptions::default();
                                opts.atol = 1e-1; opts.rtol = 1e-1;
                                match SimStepper::new(&comp_res.dae, opts) {
                                    Ok(mut stepper) => {
                                        for (name, val) in &input_defaults {
                                            let _ = stepper.set_input(name, *val);
                                        }
                                        let input_names: Vec<String> = stepper.input_names().to_vec();
                                        let symbols = collect_stepper_observables(&stepper);
                                        let descriptions = collect_variable_descriptions(&stripped_source);
                                        cached_models.insert(entity, CachedModel {
                                            session_id,
                                            model_name: model_name.clone(),
                                            source: Arc::from(source.clone()),
                                            dae: comp_res,
                                        });
                                        steppers.insert(entity, (session_id, model_name.clone(), stepper));
                                        let _ = tx_inner.send(ModelicaResult {
                                            entity, session_id, new_time: 0.0,
                                            outputs: Vec::new(),
                                            detected_symbols: symbols, error: None,
                                            log_message: Some("Parameters applied.".to_string()),
                                            is_new_model: false, is_parameter_update: true, is_reset: false,
                                            detected_input_names: input_names,
                                            detected_descriptions: descriptions,
                                        });
                                    }
                                    Err(e) => {
                                        let mut r = result_ok(entity, session_id);
                                        r.error = Some(format!("Stepper Init Error: {:?}", e));
                                        r.is_parameter_update = true;
                                        let _ = tx_inner.send(r);
                                    }
                                }
                            }
                            Err(e) => {
                                let mut r = result_ok(entity, session_id);
                                r.error = Some(format!("Re-compile Error: {:?}", e));
                                r.is_parameter_update = true;
                                let _ = tx_inner.send(r);
                            }
                        }
                    }
                    ModelicaCommand::Compile { entity, session_id, model_name, source, extra_sources, stream } => {
                        current_sessions.insert(entity, session_id);
                        if let Some(stream) = stream {
                            // Register the new lock-free publish target
                            // AND reset the previous snapshot so stale
                            // history from a prior compile doesn't bleed
                            // into the new model's horizon.
                            stream.store(Arc::new(SimSnapshot::empty_at_zero()));
                            sim_streams.insert(entity, stream);
                        }

                        // Strip input defaults so they become real runtime slots
                        let (stripped_source, input_defaults) = strip_input_defaults(&source);

                        // Loud breadcrumbs around the two opaque-and-slow
                        // steps (MSL preload + rumoca compile). Without
                        // these, the worker silently disappears for the
                        // duration — the rumoca log macros may or may
                        // not route through the workbench's tracing sink
                        // depending on Bevy's tracing-subscriber config.
                        // `bevy::log::info!` always reaches stdout.
                        let was_first_compile = compiler.is_none();
                        if was_first_compile {
                            bevy::log::info!(
                                "[worker] first-time compiler init — loading MSL into rumoca session (this can take ~10s on warm cache, minutes on cold `.cache/rumoca`)"
                            );
                        }
                        let t_init = web_time::Instant::now();
                        let compiler = compiler.get_or_insert_with(ModelicaCompiler::new);
                        if was_first_compile {
                            bevy::log::info!(
                                "[worker] compiler init done in {:.2}s",
                                t_init.elapsed().as_secs_f64(),
                            );
                        }
                        bevy::log::info!(
                            "[worker] calling compile_str for `{}` ({} bytes)",
                            model_name, stripped_source.len(),
                        );
                        let t_compile = web_time::Instant::now();
                        let _compile_outcome = if extra_sources.is_empty() {
                            compiler.compile_str(&model_name, &stripped_source, "model.mo")
                        } else {
                            compiler.compile_str_multi(&model_name, &stripped_source, "model.mo", &extra_sources)
                        };
                        bevy::log::info!(
                            "[worker] compile_str returned for `{}` in {:.2}s ({})",
                            model_name,
                            t_compile.elapsed().as_secs_f64(),
                            if _compile_outcome.is_ok() { "OK" } else { "ERR" },
                        );
                        match _compile_outcome {
                            Ok(comp_res) => {
                                let mut opts = StepperOptions::default();
                                opts.atol = 1e-1; opts.rtol = 1e-1;
                                match SimStepper::new(&comp_res.dae, opts) {
                                    Ok(mut stepper) => {
                                        // Set input defaults via set_input so they're runtime-changeable
                                        for (name, val) in &input_defaults {
                                            let _ = stepper.set_input(name, *val);
                                        }
                                        let input_names: Vec<String> = stepper.input_names().to_vec();
                                        let symbols = collect_stepper_observables(&stepper);
                                        let descriptions = collect_variable_descriptions(&stripped_source);
                                        let temp_dir = modelica_dir().join(format!("{}_{}", entity.index(), entity.generation()));
                                        let _ = std::fs::create_dir_all(&temp_dir);
                                        let temp_path = temp_dir.join("model.mo");
                                        let _ = std::fs::write(&temp_path, &source);

                                        cached_models.insert(entity, CachedModel {
                                            session_id,
                                            model_name: model_name.clone(),
                                            source: Arc::from(source.clone()),
                                            dae: comp_res,
                                        });
                                        steppers.insert(entity, (session_id, model_name.clone(), stepper));
                                        let _ = tx_inner.send(ModelicaResult {
                                            entity, session_id, new_time: 0.0,
                                            outputs: Vec::new(),
                                            detected_symbols: symbols, error: None,
                                            log_message: Some(format!("Model '{}' compiled.", model_name)),
                                            is_new_model: true, is_parameter_update: false, is_reset: false,
                                            detected_input_names: input_names,
                                            detected_descriptions: descriptions,
                                        });
                                    }
                                    Err(e) => {
                                        let mut r = result_ok(entity, session_id);
                                        r.error = Some(format!("Stepper Error: {:?}", e));
                                        // Stepper init failure during
                                        // Compile IS a compile-attempt
                                        // result — the UI classifies
                                        // and transitions state on
                                        // this flag.
                                        r.is_new_model = true;
                                        let _ = tx_inner.send(r);
                                    }
                                }
                            }
                            Err(e) => {
                                let mut r = result_ok(entity, session_id);
                                r.error = Some(format!("Compiler Error: {:?}", e));
                                r.is_new_model = true;
                                let _ = tx_inner.send(r);
                            }
                        }
                    }
                    ModelicaCommand::Step { entity, session_id, model_path, model_name, inputs, dt } => {
                        if session_id < *current_sessions.get(&entity).unwrap_or(&0) {
                            let _ = tx_inner.send(result_ok(entity, session_id));
                            return;
                        }

                        let needs_init = match steppers.get(&entity) {
                            Some((s_id, s_name, _)) => *s_id < session_id || s_name != &model_name,
                            None => true,
                        };

                        if needs_init {
                            // Try cached DAE first — recompile stripped source for input slots
                            if let Some(cached) = cached_models.get(&entity) {
                                if cached.model_name == model_name {
                                    let (stripped_source, input_defaults) = strip_input_defaults(&cached.source);
                                    let compiler = compiler.get_or_insert_with(ModelicaCompiler::new);
                                    if let Ok(comp_res) = compiler.compile_str(&cached.model_name, &stripped_source, "model.mo") {
                                        let mut opts = StepperOptions::default();
                                        opts.atol = 1e-1; opts.rtol = 1e-1;
                                        if let Ok(mut s) = SimStepper::new(&comp_res.dae, opts) {
                                            // Set input defaults first
                                            for (name, val) in &input_defaults {
                                                let _ = s.set_input(name, *val);
                                            }
                                            // Then apply any user-provided input overrides
                                            for (name, val) in &inputs {
                                                let _ = s.set_input(name, *val);
                                            }
                                            steppers.insert(entity, (session_id, model_name.clone(), s));
                                        }
                                    }
                                }
                            }
                            // Fallback: compile from file on disk
                            if !steppers.contains_key(&entity) {
                                let source = std::fs::read_to_string(&model_path).unwrap_or_default();
                                let compiler = compiler.get_or_insert_with(ModelicaCompiler::new);
                                match compiler.compile_str(&model_name, &source, &model_path.to_string_lossy()) {
                                    Ok(comp_res) => {
                                        let mut opts = StepperOptions::default();
                                        opts.atol = 1e-1; opts.rtol = 1e-1;
                                        if let Ok(mut s) = SimStepper::new(&comp_res.dae, opts) {
                                            for (name, val) in &inputs { let _ = s.set_input(name, *val); }
                                            cached_models.insert(entity, CachedModel {
                                                session_id,
                                                model_name: model_name.clone(),
                                                source: Arc::from(std::fs::read_to_string(&model_path).unwrap_or_default()),
                                                dae: comp_res,
                                            });

                                            steppers.insert(entity, (session_id, model_name.clone(), s));
                                        }
                                    }
                                    Err(e) => {
                                        let mut r = result_ok(entity, session_id);
                                        r.error = Some(format!("Initialization Failed: {:?}", e));
                                        let _ = tx_inner.send(r);
                                        return;
                                    }
                                }
                            }
                        }

                        if let Some((s_id, _, stepper)) = steppers.get_mut(&entity) {
                            if *s_id == session_id {
                                for (name, val) in inputs { let _ = stepper.set_input(&name, val); }
                                let capped_dt = dt.min(0.033); let sub_dt = capped_dt / 3.0;
                                let mut step_err = None;
                                for _ in 0..3 { if let Err(e) = stepper.step(sub_dt) { step_err = Some(e); break; } }
                                if let Some(e) = step_err {
                                    let mut r = result_ok(entity, session_id);
                                    r.new_time = stepper.time();
                                    r.error = Some(format!("Solver Error: {:?}", e));
                                    let _ = tx_inner.send(r);
                                    steppers.remove(&entity);
                                } else {
                                    // `state()` reconstructs algebraics / outputs via
                                    // `EliminationResult` and also includes inputs, so
                                    // this single call supersedes the old two-loop
                                    // variable_names + input_names collection.
                                    let outputs = collect_stepper_observables(stepper);
                                    let new_time = stepper.time();
                                    // Phase A: also publish to the
                                    // lock-free stream so consumers that
                                    // wire into it (plots, telemetry —
                                    // see TODO arch-phase-a2) can read
                                    // without main-thread round-tripping.
                                    // We continue to ship `outputs`
                                    // through the crossbeam channel as
                                    // well until plots have migrated;
                                    // once they read from `SimStream`
                                    // exclusively, the `outputs` Vec
                                    // can be cleared here to drop the
                                    // per-sample main-thread push loop.
                                    if let Some(stream) = sim_streams.get(&entity) {
                                        let prev = stream.load();
                                        let next = SimSnapshot::advance(&prev, new_time, &outputs);
                                        stream.store(Arc::new(next));
                                    }
                                    let _ = tx_inner.send(ModelicaResult {
                                        entity, session_id, new_time,
                                        outputs, error: None, log_message: None,
                                        is_new_model: false, detected_symbols: Vec::new(),
                                        is_parameter_update: false, is_reset: false,
                                        detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                                    });
                                }
                            } else {
                                let _ = tx_inner.send(result_ok(entity, session_id));
                            }
                        } else {
                            let mut r = result_ok(entity, session_id);
                            r.error = Some(
                                "No compiled model. Click Compile (or Run will compile + start)."
                                    .to_string(),
                            );
                            let _ = tx_inner.send(r);
                        }
                    }
                    ModelicaCommand::Despawn { entity } => {
                        steppers.remove(&entity);
                        cached_models.remove(&entity);
                        sim_streams.remove(&entity);
                    }
                }
            }));

            let elapsed = cmd_started.elapsed();
            // Flag anything slow enough that a user would perceive it
            // as "stuck" at WARN so it shows up even without verbose
            // logging. The 2s threshold is well above a typical MSL
            // compile (<500ms) but below "waited through it" (>5s).
            if elapsed > std::time::Duration::from_secs(2) {
                log::warn!(
                    "[worker] end: {} took {:?} (slow — possible stall)",
                    cmd_label,
                    elapsed
                );
            } else if is_hot_path {
                log::debug!("[worker] end: {} took {:?}", cmd_label, elapsed);
            } else {
                log::info!("[worker] end: {} took {:?}", cmd_label, elapsed);
            }

            if let Err(_) = result {
                let _ = tx.send(ModelicaResult {
                    entity: Entity::PLACEHOLDER,
                    session_id: 0, new_time: 0.0,
                    outputs: Vec::new(), detected_symbols: Vec::new(),
                    error: Some("Internal Worker Panic!".to_string()), log_message: None,
                    is_new_model: false, is_parameter_update: false, is_reset: false,
                    detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                });
            }
        }
    }
}

/// One-line identifier for a `ModelicaCommand`, used in worker
/// instrumentation logs. Includes the model name where available so
/// a stall can be pinned to a specific source.
fn command_label(cmd: &ModelicaCommand) -> String {
    match cmd {
        ModelicaCommand::Step { model_name, entity, .. } => {
            format!("Step model={model_name} entity={entity:?}")
        }
        ModelicaCommand::Compile { model_name, entity, .. } => {
            format!("Compile model={model_name} entity={entity:?}")
        }
        ModelicaCommand::UpdateParameters { model_name, entity, .. } => {
            format!("UpdateParameters model={model_name} entity={entity:?}")
        }
        ModelicaCommand::Reset { entity, .. } => format!("Reset entity={entity:?}"),
        ModelicaCommand::Despawn { entity } => format!("Despawn entity={entity:?}"),
    }
}

fn cmd_entity(cmd: &ModelicaCommand) -> Entity {
    match cmd {
        ModelicaCommand::Step { entity, .. } => *entity,
        ModelicaCommand::Compile { entity, .. } => *entity,
        ModelicaCommand::UpdateParameters { entity, .. } => *entity,
        ModelicaCommand::Reset { entity, .. } => *entity,
        ModelicaCommand::Despawn { entity } => *entity,
    }
}

fn cmd_session(cmd: &ModelicaCommand) -> u64 {
    match cmd {
        ModelicaCommand::Step { session_id, .. } => *session_id,
        ModelicaCommand::Compile { session_id, .. } => *session_id,
        ModelicaCommand::UpdateParameters { session_id, .. } => *session_id,
        ModelicaCommand::Reset { session_id, .. } => *session_id,
        ModelicaCommand::Despawn { .. } => 0,
    }
}

/// Returns true if two consecutive commands can be squashed (same type, same entity).
///
/// Squashing prevents "back-pressure" lag when the UI sends rapid updates
/// (e.g., dragging a parameter slider). Only the latest value is processed.
fn is_squashable(last: &ModelicaCommand, next: &ModelicaCommand) -> bool {
    match (last, next) {
        (ModelicaCommand::Step { entity: e1, .. }, ModelicaCommand::Step { entity: e2, .. }) => e1 == e2,
        (ModelicaCommand::UpdateParameters { entity: e1, .. }, ModelicaCommand::UpdateParameters { entity: e2, .. }) => e1 == e2,
        (ModelicaCommand::Compile { entity: e1, .. }, ModelicaCommand::Compile { entity: e2, .. }) => e1 == e2,
        _ => false,
    }
}

// =============================================================================
// WebAssembly Inline Worker (wasm32 only - no thread support in browser)
// =============================================================================
//
// Why this exists:
//   - std::thread::spawn panics on wasm32-unknown-unknown (no OS thread support)
//   - Web Workers are not available from Rust/wasm-bindgen without additional
//     tooling (wasm-bindgen-rayon, etc.)
//   - Instead, we process one simulation command per frame in a Bevy system.
//     This keeps the UI responsive while still running full Modelica simulation.
//
// Trade-offs:
//   - One command per frame limits throughput (fine for interactive use)
//   - No back-pressure: commands pile up in the channel if the worker falls behind
//   - All state lives in a Resource, so it resets on page reload (by design)

/// Inner simulation state for wasm32 inline worker.
/// Mirrors the local variables in `modelica_worker` on desktop.
#[cfg(target_arch = "wasm32")]
#[derive(Default)]
struct InlineWorkerInner {
    steppers: HashMap<Entity, (u64, String, SimStepper)>,
    current_sessions: HashMap<Entity, u64>,
    cached_models: HashMap<Entity, CachedModel>,
    compiler: Option<ModelicaCompiler>,
}

/// Thread-safe wrapper for wasm32 inline worker state.
///
/// SAFETY: wasm32-unknown-unknown has no threads, so Send/Sync are vacuously true.
/// SimStepper internally uses Rc<RefCell<>> which is !Send, but since no threads
/// exist on this target, we can safely implement Send/Sync.
#[cfg(target_arch = "wasm32")]
#[derive(Resource, Default)]
pub(crate) struct InlineWorker {
    inner: InlineWorkerInner,
}

#[cfg(target_arch = "wasm32")]
impl InlineWorker {
    /// Drop any previously-constructed `ModelicaCompiler`. Used by the
    /// MSL drain when the in-memory bundle finishes loading: a compiler
    /// that was lazily built before MSL was available has an empty
    /// session and would yield `unresolved type reference` for every
    /// MSL ref. The next compile will re-init via
    /// `get_or_insert_with(ModelicaCompiler::new)` and pick up the
    /// global MSL source.
    pub(crate) fn reset_compiler(&mut self) {
        self.inner.compiler = None;
    }
}

// SAFETY: wasm32-unknown-unknown has no threads, so Send/Sync are vacuously true.
#[cfg(target_arch = "wasm32")]
unsafe impl Send for InlineWorker {}
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for InlineWorker {}

/// Processes Modelica commands inline on wasm32 (no background thread).
///
/// Runs each frame in the Update schedule. Drains one command from the
/// channel and processes it synchronously, sending results back immediately.
#[cfg(target_arch = "wasm32")]
pub(crate) fn inline_worker_process(
    mut worker: ResMut<InlineWorker>,
    channels: Res<ModelicaChannels>,
) {
    let w = &mut worker.inner;
    // Process one command per frame to avoid blocking the main thread
    let Ok(cmd) = channels.rx_cmd.try_recv() else { return };

    match cmd {
        ModelicaCommand::Step { entity, session_id, model_name, inputs, dt, model_path: _ } => {
            let tx = &channels.tx_res;

            // Auto-init: compile if stepper doesn't exist
            if !w.steppers.contains_key(&entity) {
                // Try cached DAE first
                if let Some(cached) = w.cached_models.get(&entity) {
                    if cached.model_name == model_name {
                        let (stripped_source, input_defaults) = strip_input_defaults(&cached.source);
                        let compiler = w.compiler.get_or_insert_with(ModelicaCompiler::new);
                        if let Ok(comp_res) = compiler.compile_str(&cached.model_name, &stripped_source, "model.mo") {
                            let mut opts = StepperOptions::default();
                            opts.atol = 1e-1; opts.rtol = 1e-1;
                            if let Ok(mut s) = SimStepper::new(&comp_res.dae, opts) {
                                for (name, val) in &input_defaults { let _ = s.set_input(name, *val); }
                                for (name, val) in &inputs { let _ = s.set_input(name, *val); }
                                w.steppers.insert(entity, (session_id, model_name.clone(), s));
                            }
                        }
                    }
                }
                // Fallback: try to compile from model_path (won't work in web)
                // In web mode, models must be pre-compiled via Compile command first
            }

            if let Some((s_id, _, stepper)) = w.steppers.get_mut(&entity) {
                if *s_id == session_id {
                    for (name, val) in &inputs { let _ = stepper.set_input(name, *val); }
                    let capped_dt = dt.min(0.033);
                    let sub_dt = capped_dt / 3.0;
                    let mut step_err = None;
                    for _ in 0..3 { if let Err(e) = stepper.step(sub_dt) { step_err = Some(e); break; } }

                    if let Some(e) = step_err {
                        let _ = tx.send(ModelicaResult {
                            entity, session_id, new_time: stepper.time(),
                            outputs: Vec::new(),
                            detected_symbols: Vec::new(), error: Some(format!("Solver Error: {:?}", e)),
                            log_message: None, is_new_model: false, is_parameter_update: false,
                            is_reset: false, detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                        });
                        w.steppers.remove(&entity);
                    } else {
                        let outputs = collect_stepper_observables(stepper);
                        let _ = tx.send(ModelicaResult {
                            entity, session_id, new_time: stepper.time(),
                            outputs, error: None,
                            log_message: None, is_new_model: false, detected_symbols: Vec::new(),
                            is_parameter_update: false, is_reset: false, detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                        });
                    }
                } else {
                    let _ = tx.send(result_ok(entity, session_id));
                }
            } else {
                // No stepper for this entity. The Bevy-side
                // `spawn_modelica_requests` is supposed to catch this
                // and dispatch a Compile first; if we got here the
                // user pressed Run on a never-compiled model AND the
                // auto-compile hook didn't fire (e.g. doc id is
                // missing). Surface a message that tells the user
                // what to do next instead of "Sim engine failed to
                // start." which doesn't.
                let _ = tx.send(ModelicaResult {
                    entity, session_id, new_time: 0.0,
                    outputs: Vec::new(),
                    detected_symbols: Vec::new(),
                    error: Some(
                        "No compiled model. Click Compile (or Run will compile + start)."
                            .to_string(),
                    ),
                    log_message: None, is_new_model: false, is_parameter_update: false,
                    is_reset: false, detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                });
            }
        }
        ModelicaCommand::Compile { entity, session_id, model_name, source, extra_sources, stream: _stream } => {
            // NB: the wasm inline worker runs on the Bevy main thread
            // today and does not publish to a lock-free SimStream.
            // Phase A lands on desktop first; TODO(arch-phase-b) wire
            // the wasm path once the inline worker moves off-thread.
            w.current_sessions.insert(entity, session_id);
            let (stripped_source, input_defaults) = strip_input_defaults(&source);

            let mut opts = StepperOptions::default();
            opts.atol = 1e-1; opts.rtol = 1e-1;
            let tx = &channels.tx_res;

            let compiler = w.compiler.get_or_insert_with(ModelicaCompiler::new);
            let compile_outcome = if extra_sources.is_empty() {
                compiler.compile_str(&model_name, &stripped_source, "model.mo")
            } else {
                compiler.compile_str_multi(&model_name, &stripped_source, "model.mo", &extra_sources)
            };
            match compile_outcome {
                Ok(comp_res) => {
                    match SimStepper::new(&comp_res.dae, opts) {
                        Ok(mut stepper) => {
                            for (name, val) in &input_defaults { let _ = stepper.set_input(name, *val); }
                            let input_names: Vec<String> = stepper.input_names().to_vec();
                            let symbols = collect_stepper_observables(&stepper);
                            let descriptions = collect_variable_descriptions(&stripped_source);
                            w.cached_models.insert(entity, CachedModel {
                                session_id, model_name: model_name.clone(), source: Arc::from(source.clone()),
                                dae: comp_res.clone(),
                            });

                            w.steppers.insert(entity, (session_id, model_name.clone(), stepper));
                            let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: symbols, error: None,
                                log_message: Some("Compiled successfully.".to_string()),
                                is_new_model: true, is_parameter_update: false, is_reset: false,
                                detected_input_names: input_names,
                                detected_descriptions: descriptions,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: Vec::new(), error: Some(format!("Stepper Init Error: {:?}", e)),
                                log_message: None, is_new_model: true, is_parameter_update: false, is_reset: false,
                                detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(ModelicaResult {
                        entity, session_id, new_time: 0.0,
                        outputs: Vec::new(),
                        detected_symbols: Vec::new(), error: Some(format!("Compile Error: {:?}", e)),
                        log_message: None, is_new_model: true, is_parameter_update: false, is_reset: false,
                        detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                    });
                }
            }
        }
        ModelicaCommand::Reset { entity, session_id } => {
            w.current_sessions.insert(entity, session_id);
            let tx = &channels.tx_res;

            if let Some(cached) = w.cached_models.get(&entity) {
                let (stripped_source, input_defaults) = strip_input_defaults(&cached.source);
                let mut opts = StepperOptions::default();
                opts.atol = 1e-1; opts.rtol = 1e-1;
                let compiler = w.compiler.get_or_insert_with(ModelicaCompiler::new);
                match compiler.compile_str(&cached.model_name, &stripped_source, "model.mo") {
                    Ok(comp_res) => {
                        if let Ok(mut stepper) = SimStepper::new(&comp_res.dae, opts) {
                            for (name, val) in &input_defaults { let _ = stepper.set_input(name, *val); }
                            let input_names: Vec<String> = stepper.input_names().to_vec();
                            let symbols = collect_stepper_observables(&stepper);
                            let descriptions = collect_variable_descriptions(&stripped_source);
                            w.steppers.insert(entity, (session_id, cached.model_name.clone(), stepper));
                            let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: symbols, error: None,
                                log_message: Some("Reset complete.".to_string()),
                                is_new_model: false, is_parameter_update: false, is_reset: true,
                                detected_input_names: input_names,
                                detected_descriptions: descriptions,
                            });

                                } else {
                                let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: Vec::new(), error: Some("Stepper init failed".to_string()),
                                log_message: None, is_new_model: false, is_parameter_update: false, is_reset: true,
                                detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                                });
                                }
                                }
                                Err(e) => {
                                let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: Vec::new(), error: Some(format!("Reset compile error: {:?}", e)),
                                log_message: None, is_new_model: false, is_parameter_update: false, is_reset: true,
                                detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                                });
                                }
                                }
                                } else {
                                w.steppers.remove(&entity);
                                let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: Vec::new(), error: None,
                                log_message: Some("Reset complete (no cached model).".to_string()),
                                is_new_model: false, is_parameter_update: false, is_reset: true,
                                detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                                });
                                }

        }
        ModelicaCommand::UpdateParameters { entity, session_id, model_name, source } => {
            if session_id < *w.current_sessions.get(&entity).unwrap_or(&0) {
                let _ = channels.tx_res.send(result_ok(entity, session_id));
                return;
            }
            w.current_sessions.insert(entity, session_id);
            let (stripped_source, input_defaults) = strip_input_defaults(&source);

            let mut opts = StepperOptions::default();
            opts.atol = 1e-1; opts.rtol = 1e-1;
            let tx = &channels.tx_res;

            let compiler = w.compiler.get_or_insert_with(ModelicaCompiler::new);
            match compiler.compile_str(&model_name, &stripped_source, "model.mo") {
                Ok(comp_res) => {
                    match SimStepper::new(&comp_res.dae, opts) {
                        Ok(mut stepper) => {
                            for (name, val) in &input_defaults { let _ = stepper.set_input(name, *val); }
                            let input_names: Vec<String> = stepper.input_names().to_vec();
                            let symbols = collect_stepper_observables(&stepper);
                            let descriptions = collect_variable_descriptions(&stripped_source);
                            w.cached_models.insert(entity, CachedModel {
                                session_id, model_name: model_name.clone(), source: Arc::from(source.clone()),
                                dae: comp_res,
                            });

                            w.steppers.insert(entity, (session_id, model_name.clone(), stepper));
                            let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: symbols, error: None,
                                log_message: Some("Parameters applied.".to_string()),
                                is_new_model: false, is_parameter_update: true, is_reset: false,
                                detected_input_names: input_names,
                                detected_descriptions: descriptions,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(ModelicaResult {
                                entity, session_id, new_time: 0.0,
                                outputs: Vec::new(),
                                detected_symbols: Vec::new(), error: Some(format!("Stepper Init Error: {:?}", e)),
                                log_message: None, is_new_model: false, is_parameter_update: true, is_reset: false,
                                detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                            });
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(ModelicaResult {
                        entity, session_id, new_time: 0.0,
                        outputs: Vec::new(),
                        detected_symbols: Vec::new(), error: Some(format!("Re-compile Error: {:?}", e)),
                        log_message: None, is_new_model: false, is_parameter_update: true, is_reset: false,
                        detected_input_names: Vec::new(), detected_descriptions: Vec::new(),
                    });
                }
            }
        }
        ModelicaCommand::Despawn { entity } => {
            w.steppers.remove(&entity);
            w.cached_models.remove(&entity);
        }
    }
}

/// Component that attaches a Modelica model to an entity.
///
/// Holds the model path, name, session ID, parameters, inputs, and observable variables.
/// The `is_stepping` flag prevents duplicate Step commands while waiting for results.
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ModelicaModel {
    pub model_path: PathBuf,
    pub model_name: String,
    pub current_time: f64,
    pub last_step_time: f64,
    pub session_id: u64,
    pub paused: bool,
    /// Tunable constants (parameter Real ...)
    pub parameters: HashMap<String, f64>,
    /// Control inputs (input Real ...)
    pub inputs: HashMap<String, f64>,
    /// All other observable variables (Real soc, etc)
    pub variables: HashMap<String, f64>,
    /// Per-variable description strings lifted from the Modelica source
    /// (MLS §A.2.5). Populated on compile-type results so the UI can
    /// render them as hover tooltips in Telemetry, Inspector, Diagram,
    /// etc. Not reflected — these are derived from the source and can
    /// be recomputed on reload.
    #[reflect(ignore)]
    pub descriptions: HashMap<String, String>,
    /// Per-parameter `(min, max)` bounds lifted from Modelica
    /// `parameter Real x(min=..., max=...) = ...` declarations.
    /// `None` at either end means unbounded on that side. The
    /// Telemetry panel clamps the DragValue to this range so users
    /// can't push a model out of its authored operating envelope.
    #[reflect(ignore)]
    pub parameter_bounds: HashMap<String, (Option<f64>, Option<f64>)>,
    /// Canonical id of the Modelica source document backing this entity,
    /// looked up in [`ui::ModelicaDocumentRegistry`]. `DocumentId::default()`
    /// (`0`) means "no document assigned yet"; systems should treat it as
    /// a miss. Not reflected — ids are session-local allocations, not
    /// scene-serializable.
    #[reflect(ignore)]
    pub document: lunco_doc::DocumentId,
    /// `true` while a `Step` request is in flight to the worker.
    /// Cleared when the response arrives in
    /// [`handle_modelica_responses`]. Distinct from
    /// [`Self::is_compiling`] — a long-running compile must NOT count
    /// as a hung step (that conflation is what made the dispatcher's
    /// "worker hung?" warning spam every frame for the duration of a
    /// slow Modelica compile).
    #[reflect(ignore)]
    pub is_stepping: bool,
    /// `true` while a `Compile` request is in flight to the worker.
    /// Set by the `CompileModel` observer, cleared when a compile-
    /// shaped result (`is_new_model` / `is_parameter_update`) lands.
    /// Compiles can take seconds (occasionally minutes for MSL-heavy
    /// examples); the dispatcher uses this to suppress its
    /// step-hang warning while a compile is legitimately running.
    #[reflect(ignore)]
    pub is_compiling: bool,
    /// `true` after a successful Compile has installed a stepper for
    /// this entity in the Modelica worker. `spawn_modelica_requests`
    /// uses this to dispatch a Compile (instead of a doomed Step) when
    /// the user clicks Run on a never-compiled model. Reset to `false`
    /// when a result reports an error or a fresh Compile is in flight.
    #[reflect(ignore)]
    pub is_compiled: bool,
}

/// Sends `Step` commands for each active model.
///
/// Runs in [`FixedUpdate`] using the fixed timestep delta. All models step with
/// the same dt, matching Avian physics and wire propagation.
pub fn spawn_modelica_requests(
    channels: Res<ModelicaChannels>,
    time: Res<Time<Fixed>>,
    mut q_models: Query<(Entity, &mut ModelicaModel)>,
    mut commands: Commands,
) {
    let dt = time.delta_secs_f64();

    for (entity, mut model) in q_models.iter_mut() {
        if model.is_stepping {
            continue;
        }
        if model.paused {
            continue;
        }

        // First-step path: model has been unpaused (user pressed Run)
        // but no Compile has succeeded yet — the worker has no stepper
        // and a Step would just bounce back as "Click Compile first".
        // Auto-trigger CompileModel instead. The observer flips
        // `is_stepping = true` and bumps `session_id`, so we won't
        // re-trigger on subsequent frames; on a successful result the
        // response handler sets `is_compiled = true` and unpauses.
        if !model.is_compiled {
            let doc = model.document;
            if doc != lunco_doc::DocumentId::default() {
                commands.trigger(crate::ui::commands::CompileModel {
                    doc,
                    class: if model.model_name.is_empty() {
                        None
                    } else {
                        Some(model.model_name.clone())
                    },
                });
            }
            // Don't ship a Step this frame either way — let the
            // compile flow run.
            continue;
        }

        let inputs: Vec<(String, f64)> = model.inputs.iter()
            .map(|(name, val)| (name.clone(), *val))
            .collect();

        model.is_stepping = true;
        let _ = channels.tx.send(ModelicaCommand::Step {
            entity,
            session_id: model.session_id,
            model_path: model.model_path.clone(),
            model_name: model.model_name.clone(),
            inputs,
            dt,
        });
    }
}

/// System that processes results from the background worker.
///
/// Updates `ModelicaModel` components with fresh simulation outputs, handles
/// session fencing to ignore stale results, and manages `WorkbenchState` for
/// UI display. On `is_new_model`, clears old data and unpauses the simulation.
pub fn handle_modelica_responses(
    channels: Res<ModelicaChannels>,
    mut q_models: Query<&mut ModelicaModel>,
    mut workbench_state: ResMut<crate::ui::WorkbenchState>,
    // Headless callers (e.g. cosim tests) run this system without the
    // UI plugin, so the console + compile-state resources may be
    // absent. Make both optional so the core stepping path survives
    // those setups without forcing them to pull in the UI module.
    compile_states: Option<ResMut<crate::ui::CompileStates>>,
    console: Option<ResMut<crate::ui::panels::console::ConsoleLog>>,
    // Optional — a headless cosim harness may skip `LuncoVizPlugin`
    // entirely. When present, every outgoing sample is published into
    // the registry, and the default Modelica plot's bindings are
    // seeded on first compile of each entity.
    mut signals: Option<ResMut<lunco_viz::SignalRegistry>>,
    mut viz_registry: Option<ResMut<lunco_viz::VisualizationRegistry>>,
) {
    let mut compile_states = compile_states;
    let mut console = console;
    while let Ok(result) = channels.rx.try_recv() {
        if result.entity == Entity::PLACEHOLDER {
            let msg = "Simulation worker crashed and restarted.";
            warn!("{msg}");
            if let Some(c) = console.as_mut() {
                c.error(msg);
            }
            continue;
        }

        if let Ok(mut model) = q_models.get_mut(result.entity) {
            // ALWAYS check session ID before resetting is_stepping
            // Stale results must NOT reset the flag.
            if result.session_id < model.session_id { continue; }

            model.is_stepping = false;
            // Compile-shaped results (new model / parameter update /
            // reset) close out the corresponding `is_compiling` window
            // the `CompileModel` observer opened. Step results don't
            // touch this flag — they were never compile-flagged.
            if result.is_new_model || result.is_parameter_update || result.is_reset {
                model.is_compiling = false;
            }

            // Forward log messages to console via bevy_workbench's console system
            if let Some(msg) = &result.log_message {
                info!("[Modelica] {msg}");
                // Only forward lifecycle notes (compile / reset / param
                // update). Skip the per-Step logs so the console doesn't
                // flood at 60 Hz.
                if result.is_new_model || result.is_reset || result.is_parameter_update {
                    if let Some(c) = console.as_mut() {
                        c.info(format!("[{}] {msg}", model.model_name));
                    }
                }
            }

            // Transition compile state for this entity's document, but
            // only on compile-style results (new-model / parameter-update).
            // Step results arrive continuously and must not clobber
            // Ready/Error classifications.
            let is_compile_result = result.is_new_model || result.is_parameter_update;
            if is_compile_result && !model.document.is_unassigned() {
                let new_state = if result.error.is_some() {
                    crate::ui::CompileState::Error
                } else {
                    crate::ui::CompileState::Ready
                };
                if let Some(cs) = compile_states.as_mut() {
                    let elapsed = cs.mark_finished(model.document, new_state);
                    if let Some(dur) = elapsed {
                        let ms = dur.as_secs_f64() * 1000.0;
                        let human = if ms >= 1000.0 {
                            format!("{:.2} s", ms / 1000.0)
                        } else {
                            format!("{:.0} ms", ms)
                        };
                        match new_state {
                            crate::ui::CompileState::Error => {
                                warn!(
                                    "[Modelica] Compile finished with error for `{}` in {}",
                                    model.model_name, human
                                );
                                if let Some(c) = console.as_mut() {
                                    c.error(format!(
                                        "⏹ Compile FAILED: '{}' in {}",
                                        model.model_name, human
                                    ));
                                }
                            }
                            crate::ui::CompileState::Ready => {
                                info!(
                                    "[Modelica] Compile finished for `{}` in {}",
                                    model.model_name, human
                                );
                                if let Some(c) = console.as_mut() {
                                    c.info(format!(
                                        "✓ Compile finished: '{}' in {}",
                                        model.model_name, human
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Variable description strings for hover tooltips (Telemetry,
            // Inspector, Diagram). Populated on compile-type results only;
            // step results leave `detected_descriptions` empty so we
            // don't blow away the map on every step.
            if (result.is_new_model || result.is_parameter_update || result.is_reset)
                && !result.detected_descriptions.is_empty()
            {
                model.descriptions.clear();
                for (name, desc) in &result.detected_descriptions {
                    model.descriptions.insert(name.clone(), desc.clone());
                }
            }

            if let Some(err) = &result.error {
                workbench_state.compilation_error = Some(err.clone());
                warn!("[Modelica] {err}");
                // Classify for the console: compile-time errors are
                // distinct from solver blowups during Step. Both are
                // Error-level; the prefix tells the user where it came
                // from at a glance.
                let prefix = if result.is_new_model {
                    "Compile error"
                } else if result.is_parameter_update {
                    "Parameter update error"
                } else if result.is_reset {
                    "Reset error"
                } else {
                    "Solver error"
                };
                if let Some(c) = console.as_mut() {
                    c.error(format!("[{}] {prefix}: {err}", model.model_name));
                }
                model.paused = true;
                // Solver errors destroy the stepper in the worker
                // (lib.rs ~1176 removes it). Clear the flag so the
                // next Run after the user fixes things triggers a
                // fresh Compile rather than a doomed Step. Compile
                // errors flip this in the `is_new_model` block below.
                model.is_compiled = false;
            } else if workbench_state.selected_entity == Some(result.entity) {
                workbench_state.compilation_error = None;
            }

            if result.is_new_model {
                model.model_path = modelica_dir()
                    .join(format!("{}_{}", result.entity.index(), result.entity.generation()))
                    .join("model.mo");
                model.variables.clear();
                // Only unpause on a *successful* Compile. A failed
                // Compile leaves the stepper empty, and unpausing would
                // cause `spawn_modelica_requests` to ship a Step →
                // worker recompiles from scratch (~10s) → error → repeat
                // forever. The earlier error-branch `paused = true`
                // marks the model as blocked; the user resumes
                // explicitly after fixing the source.
                if result.error.is_none() {
                    model.paused = false;
                    // Worker has installed a stepper for this entity.
                    // `spawn_modelica_requests` reads this to decide
                    // whether to ship Step or trigger Compile-on-first-step.
                    model.is_compiled = true;
                } else {
                    model.is_compiled = false;
                }

                // Merge input names from the worker with values the UI already extracted from source.
                // The UI extracts defaults from source code (e.g., `input Real g = 9.81` → g: 9.81),
                // which is more reliable than the worker's DAE-discovered names (which may have 0.0).
                let ui_inputs: HashMap<String, f64> = std::mem::take(&mut model.inputs);
                for name in &result.detected_input_names {
                    model.inputs.entry(name.clone())
                        .or_insert_with(|| *ui_inputs.get(name).unwrap_or(&0.0));
                }
                for (name, val) in ui_inputs {
                    model.inputs.entry(name).or_insert(val);
                }

                model.current_time = 0.0;
                model.last_step_time = 0.0;
            } else if result.is_parameter_update {
                model.current_time = 0.0;
                model.last_step_time = 0.0;
            } else if result.is_reset {
                model.current_time = 0.0;
                model.last_step_time = 0.0;
                model.variables.clear();
                // Preserve inputs and parameters
            }

            // Update observable variables from detected symbols and step outputs
            for (name, val) in result.detected_symbols.iter().chain(result.outputs.iter()) {
                if !model.inputs.contains_key(name) && !model.parameters.contains_key(name) {
                    model.variables.insert(name.clone(), *val);
                }
            }

            model.current_time = result.new_time;
            model.last_step_time = result.new_time;
            let time_val = result.new_time;

            // Publish every outgoing scalar sample into the global
            // `SignalRegistry`. The Graphs panel and any future
            // visualization (Avian / USD / scripts) read uniformly
            // from here — there is no longer a Modelica-specific
            // shadow history.
            if let Some(sigs) = signals.as_deref_mut() {
                // Compile-type results reset the signal's horizon so
                // the old run's tail doesn't bleed into the new one.
                if result.is_new_model || result.is_reset || result.is_parameter_update {
                    for (name, _) in result.detected_symbols.iter().chain(result.outputs.iter()) {
                        sigs.clear_history(&lunco_viz::SignalRef::new(
                            result.entity,
                            name.clone(),
                        ));
                    }
                }
                for (name, val) in result.outputs.iter().chain(result.detected_symbols.iter()) {
                    sigs.push_scalar(
                        lunco_viz::SignalRef::new(result.entity, name.clone()),
                        time_val,
                        *val,
                    );
                }
                // Publish / refresh description metadata on compile-
                // type results so the viz inspector can show tooltips
                // sourced the same way Telemetry does today.
                if result.is_new_model || result.is_parameter_update {
                    for (name, desc) in &result.detected_descriptions {
                        sigs.update_meta(
                            lunco_viz::SignalRef::new(result.entity, name.clone()),
                            lunco_viz::SignalMeta {
                                description: Some(desc.clone()),
                                unit: None,
                                provenance: Some("modelica".to_string()),
                            },
                        );
                    }
                }
            }

            // Auto-seed the default Modelica plot with every observable
            // from a freshly-compiled model. Preserves the pre-viz UX
            // where compiling immediately filled the graph with all
            // the model's observables. Does nothing when the user has
            // already curated the bindings.
            if result.is_new_model {
                if let Some(reg) = viz_registry.as_deref_mut() {
                    // Clear stale bindings from any prior model/entity so
                    // switching models doesn't leave old signals plotted.
                    // We deliberately do *not* auto-bind every detected
                    // observable any more — a freshly compiled model
                    // starts with an *empty* default plot. Users add
                    // signals via the Telemetry panel checkboxes (or
                    // place embedded `__LunCo_PlotNode` tiles on the
                    // diagram). Avoids the noisy "12 lines on launch"
                    // experience that prompted users to manually
                    // un-tick everything before they could see what
                    // they cared about.
                    if let Some(cfg) = reg.get_mut(crate::ui::viz::DEFAULT_MODELICA_GRAPH) {
                        cfg.inputs.clear();
                    }
                    let _ = result.entity;
                    let _ = result.detected_symbols.len();
                    let _ = model.parameters.len();
                }
            }
        }
    }
}