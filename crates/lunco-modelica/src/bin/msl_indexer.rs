//! CLI wrapper for the MSL indexer.
//!
//! The actual indexer lives in `lunco_modelica::indexer` as a library
//! entry point so the workbench can drive the same workflow in-process
//! on `AsyncComputeTaskPool` after a fresh MSL download. This binary
//! is a thin shim: parse CLI args, hand off to `indexer::run`.

fn main() {
    let opts = lunco_modelica::indexer::Options::parse();
    lunco_modelica::indexer::run(opts);
}
