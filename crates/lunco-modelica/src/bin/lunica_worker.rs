//! Off-thread Modelica worker — wasm32-unknown-unknown only.
//!
//! Runs inside a Web Worker with its own wasm linear memory. Listens for
//! bincode-serialized `ModelicaCommand` messages from the main page, drives
//! them through the same `worker::process_inline_command` dispatch the inline
//! path uses, and `postMessage`s each `ModelicaResult` back.
//!
//! Why a separate bin
//! ------------------
//! `wasm32-unknown-unknown` has no OS threads, so any rumoca compile that
//! takes seconds blocks the UI. Putting the dispatch behind a Web Worker —
//! which is a separate JS thread with a separate wasm instance — moves the
//! blocking work off the page's main thread without needing nightly Rust
//! atomics or `SharedArrayBuffer`. The native build is unchanged: it still
//! uses `worker::modelica_worker` on a real `std::thread`.
//!
//! State
//! -----
//! One `InlineWorkerInner` per worker bundle; lives for the lifetime of the
//! page. State (steppers, DAE cache, lazy `ModelicaCompiler`) survives across
//! postMessage round-trips so back-to-back Step commands hit the warm
//! stepper without any re-compile cost.
//!
//! MSL
//! ---
//! TODO(arch-msl-handoff): the worker needs MSL to be present in its own
//! `GLOBAL_PARSED_MSL` slot before the first Compile resolves any
//! `Modelica.*` reference. The minimum-viable path is to have the main
//! app send an `InstallParsedMsl(Vec<(String, StoredDefinition)>)` envelope
//! to the worker once its own MSL fetch lands; the worker decodes and
//! installs. That requires a `WireMessage` envelope around `ModelicaCommand`
//! (variants: `Command(ModelicaCommand)` / `InstallMsl(...)`). Until that's
//! wired, the worker compiles will fail with "unresolved reference
//! Modelica.*" — the channel architecture is still verifiable by sending a
//! Compile of a self-contained model that doesn't reference MSL.

// Wasm32-only binary; the desktop stub below keeps `cargo build` for the
// host target passing without producing a meaningful executable.
fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    panic!("lunica_worker is wasm32-only — built into a Web Worker bundle by scripts/build_web.sh.");
}

#[cfg(target_arch = "wasm32")]
mod wasm {
use std::cell::RefCell;

use js_sys::Uint8Array;
use lunco_modelica::worker::{ModelicaCommand, ModelicaResult};
use lunco_modelica::worker_transport::{WireMessage, WireResult};

fn command_label(cmd: &ModelicaCommand) -> String {
    match cmd {
        ModelicaCommand::Step { model_name, entity, .. } => format!("Step {model_name} entity={entity:?}"),
        ModelicaCommand::Compile { model_name, entity, .. } => format!("Compile {model_name} entity={entity:?}"),
        ModelicaCommand::UpdateParameters { model_name, entity, .. } => format!("UpdateParameters {model_name} entity={entity:?}"),
        ModelicaCommand::Reset { entity, .. } => format!("Reset entity={entity:?}"),
        ModelicaCommand::Despawn { entity } => format!("Despawn entity={entity:?}"),
    }
}
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

use lunco_modelica::worker::{process_inline_command, InlineWorkerInner};

thread_local! {
    /// Per-worker dispatch state. Outlives any single message because rumoca
    /// session caches and the lazy `ModelicaCompiler` are expensive to
    /// rebuild.
    static STATE: RefCell<InlineWorkerInner> = RefCell::new(InlineWorkerInner::default());

    /// Holds the `onmessage` closure for the lifetime of the worker; dropping
    /// it would un-register the JS-side handler.
    static ONMESSAGE_CB: RefCell<Option<Closure<dyn FnMut(MessageEvent)>>> = RefCell::new(None);
}

fn worker_global() -> DedicatedWorkerGlobalScope {
    js_sys::global()
        .dyn_into::<DedicatedWorkerGlobalScope>()
        .expect("running outside a DedicatedWorker context")
}

fn post_wire(scope: &DedicatedWorkerGlobalScope, msg: &WireResult) {
    let bytes = match bincode::serialize(msg) {
        Ok(b) => b,
        Err(e) => {
            web_sys::console::error_1(
                &format!("[lunica_worker] serialize wire failed: {e}").into(),
            );
            return;
        }
    };
    let array = Uint8Array::new_with_length(bytes.len() as u32);
    array.copy_from(&bytes);
    if let Err(e) = scope.post_message(&array) {
        web_sys::console::error_1(
            &format!("[lunica_worker] post_message failed: {e:?}").into(),
        );
    }
}

fn post_result(scope: &DedicatedWorkerGlobalScope, result: ModelicaResult) {
    post_wire(scope, &WireResult::Result(result));
}

fn post_log(scope: &DedicatedWorkerGlobalScope, line: impl Into<String>) {
    post_wire(scope, &WireResult::Log(line.into()));
}

#[wasm_bindgen(start)]
pub fn run() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    web_sys::console::log_1(&"[lunica_worker] starting".into());

    let scope = worker_global();
    let scope_for_cb = scope.clone();

    let onmessage = Closure::wrap(Box::new(move |event: MessageEvent| {
        let bytes: Vec<u8> = match Uint8Array::new(&event.data()).to_vec() {
            v if !v.is_empty() => v,
            _ => return,
        };
        let envelope: WireMessage = match bincode::deserialize(&bytes) {
            Ok(c) => c,
            Err(e) => {
                web_sys::console::error_1(
                    &format!("[lunica_worker] decode message failed: {e}").into(),
                );
                return;
            }
        };

        match envelope {
            WireMessage::Command(cmd) => {
                let scope = scope_for_cb.clone();
                let label = command_label(&cmd);
                let started = web_time::Instant::now();
                post_log(&scope, format!("recv: {label}"));
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    STATE.with(|s| {
                        let mut state = s.borrow_mut();
                        process_inline_command(&mut state, cmd, |result| {
                            post_result(&scope, result);
                        });
                    });
                }));
                match outcome {
                    Ok(()) => {
                        post_log(
                            &scope,
                            format!(
                                "done: {label} in {:.2}s",
                                started.elapsed().as_secs_f64()
                            ),
                        );
                    }
                    Err(e) => {
                        let msg = e
                            .downcast_ref::<&'static str>()
                            .copied()
                            .or_else(|| e.downcast_ref::<String>().map(|s| s.as_str()))
                            .unwrap_or("(unknown panic payload)");
                        post_log(
                            &scope,
                            format!(
                                "PANIC during {label} after {:.2}s: {msg}",
                                started.elapsed().as_secs_f64()
                            ),
                        );
                    }
                }
            }
            WireMessage::InstallParsedMsl(parsed) => {
                let count = parsed.len();
                let started = web_time::Instant::now();
                lunco_modelica::msl_remote::install_global_parsed_msl_pub(parsed);
                post_log(
                    &scope_for_cb,
                    format!(
                        "installed MSL: {count} docs in {:.2}s",
                        started.elapsed().as_secs_f64()
                    ),
                );
            }
            WireMessage::Ping(tag) => {
                post_log(
                    &scope_for_cb,
                    format!(
                        "pong: {tag} (msl={})",
                        lunco_modelica::msl_remote::global_parsed_msl()
                            .map(|m| m.len())
                            .unwrap_or(0)
                    ),
                );
            }
        }
    }) as Box<dyn FnMut(MessageEvent)>);

    scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    ONMESSAGE_CB.with(|slot| {
        *slot.borrow_mut() = Some(onmessage);
    });

    // Echo a hello back to main so the page knows the worker
    // wasm finished init and onmessage is wired. Without this the only
    // way to know the worker came up was to send a ping; if anything
    // panicked during init the page just silently never got results.
    post_log(&scope, "ready (worker wasm init complete)");
    web_sys::console::log_1(&"[lunica_worker] ready".into());
    Ok(())
}
} // mod wasm
