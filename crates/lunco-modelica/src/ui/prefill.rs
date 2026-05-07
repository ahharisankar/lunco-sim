//! Pre-compile seeding of `ModelicaModel` from the freshly-parsed AST.
//!
//! Runs after `drive_engine_sync` each tick. For every open document
//! whose syntax generation has advanced past the cursor, spawn (or
//! refresh while not yet compiled) a `ModelicaModel` linked to the
//! doc, populated from the AST. Lets Telemetry, Graphs, and the
//! canvas plot menus work pre-compile with no per-panel fallback.

use bevy::prelude::*;
use std::collections::HashMap;

use crate::ast_extract;
use lunco_doc::DocumentId;
use crate::ui::state::ModelicaDocumentRegistry;
use crate::ModelicaModel;

#[derive(Resource, Default)]
pub struct PrefillCursor {
    last_seeded: HashMap<DocumentId, u64>,
}

pub fn prefill_models_from_ast(
    mut commands: Commands,
    mut registry: ResMut<ModelicaDocumentRegistry>,
    mut cursor: ResMut<PrefillCursor>,
    mut q_models: Query<&mut ModelicaModel>,
    engine: Option<Res<crate::engine_resource::ModelicaEngineHandle>>,
) {
    struct Plan {
        doc: DocumentId,
        gen: u64,
        model_name: String,
        parameters: HashMap<String, f64>,
        parameter_bounds: HashMap<String, (Option<f64>, Option<f64>)>,
        inputs_with_defaults: HashMap<String, f64>,
        runtime_inputs: Vec<String>,
        // `(qualified_name, optional_start_value)` — `tank.m` etc.
        // matching what the simulator publishes post-compile.
        // Composite components (`tank: Tank`) are filtered out by
        // the walker; only scalar leaves end up here.
        flat_variables: Vec<(String, Option<f64>)>,
        descriptions: HashMap<String, String>,
    }
    let mut plans: Vec<Plan> = Vec::new();
    for (doc_id, host) in registry.iter() {
        let document = host.document();
        let gen = document.syntax().generation;
        if gen == 0 || cursor.last_seeded.get(&doc_id).copied().unwrap_or(0) >= gen {
            continue;
        }
        let ast = document.syntax().ast();
        let Some(model_name) = ast_extract::extract_model_name_from_ast(ast) else {
            continue;
        };
        let parameters = ast_extract::extract_parameters_from_ast(ast);
        // Flatten through `tank: Tank → Real m(start = m_initial)`
        // → `tank.m = 4000` so the canvas icons (which substitute
        // `%tank.m`) and any plot binding find the right key
        // pre-compile. Falls back to leaf names from the
        // single-class extractor for any variable the flatten
        // missed (root class's own components, or types not in
        // this doc).
        let root = model_name
            .rsplit('.')
            .next()
            .unwrap_or(&model_name)
            .to_string();
        // Cross-doc resolver: any type the in-doc walker misses
        // (e.g. `FluidPort` declared in a sibling open file or in
        // MSL) goes through the workspace engine's `class_def`.
        // Without this, connector subvariables like `valve.port_a.p`
        // never appear pre-compile.
        let flat_variables = if let Some(handle) = engine.as_ref() {
            ast_extract::extract_flat_variables_with_resolver(
                ast,
                &root,
                &parameters,
                |type_qualified| handle.lock().class_def(type_qualified),
            )
        } else {
            ast_extract::extract_flat_variables_from_ast(ast, &root, &parameters)
        };
        plans.push(Plan {
            doc: doc_id,
            gen,
            model_name,
            parameters,
            parameter_bounds: ast_extract::extract_parameter_bounds_from_ast(ast),
            inputs_with_defaults: ast_extract::extract_inputs_with_defaults_from_ast(ast),
            runtime_inputs: ast_extract::extract_input_names_from_ast(ast),
            flat_variables,
            descriptions: ast_extract::extract_descriptions(document.source())
                .into_iter()
                .collect(),
        });
    }

    for plan in plans {
        let linked = registry.entities_linked_to(plan.doc);
        if let Some(&entity) = linked.first() {
            // Refresh fields only while the model hasn't been
            // compiled. Once compiled, the worker owns truth and the
            // user's tuning of parameters lives in the component —
            // re-seeding from AST defaults would undo their edits.
            if let Ok(mut model) = q_models.get_mut(entity) {
                if !model.is_compiled {
                    model.model_name = plan.model_name.clone();
                    model.parameters = plan.parameters;
                    model.parameter_bounds = plan.parameter_bounds;
                    model.inputs.clear();
                    for (name, val) in &plan.inputs_with_defaults {
                        model.inputs.insert(name.clone(), *val);
                    }
                    for name in &plan.runtime_inputs {
                        model.inputs.entry(name.clone()).or_insert(0.0);
                    }
                    model.variables.clear();
                    // Only the flattened qualified leaves end up in
                    // `variables`. Composite component instances
                    // (e.g. `tank: Tank`, `engine: Engine`) are not
                    // observable scalars — the walker recurses into
                    // them to emit `tank.m`, `engine.thrust`, etc.,
                    // matching what the simulator publishes.
                    for (qname, start) in &plan.flat_variables {
                        model.variables.insert(qname.clone(), start.unwrap_or(0.0));
                    }
                    model.descriptions = plan.descriptions;
                }
            }
        } else {
            let mut inputs: HashMap<String, f64> = HashMap::new();
            for (name, val) in &plan.inputs_with_defaults {
                inputs.insert(name.clone(), *val);
            }
            for name in &plan.runtime_inputs {
                inputs.entry(name.clone()).or_insert(0.0);
            }
            let mut variables: HashMap<String, f64> = HashMap::new();
            for (qname, start) in &plan.flat_variables {
                variables.insert(qname.clone(), start.unwrap_or(0.0));
            }
            let entity = commands
                .spawn((
                    Name::new(plan.model_name.clone()),
                    ModelicaModel {
                        model_path: "".into(),
                        model_name: plan.model_name.clone(),
                        current_time: 0.0,
                        last_step_time: 0.0,
                        session_id: 1,
                        paused: true,
                        parameters: plan.parameters,
                        parameter_bounds: plan.parameter_bounds,
                        inputs,
                        variables,
                        descriptions: plan.descriptions,
                        document: plan.doc,
                        is_stepping: false,
                        is_compiling: false,
                        is_compiled: false,
                    },
                ))
                .id();
            registry.link(entity, plan.doc);
        }
        cursor.last_seeded.insert(plan.doc, plan.gen);
    }
}
