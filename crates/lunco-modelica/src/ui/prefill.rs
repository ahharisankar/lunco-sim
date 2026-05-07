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
) {
    struct Plan {
        doc: DocumentId,
        gen: u64,
        model_name: String,
        parameters: HashMap<String, f64>,
        parameter_bounds: HashMap<String, (Option<f64>, Option<f64>)>,
        inputs_with_defaults: HashMap<String, f64>,
        runtime_inputs: Vec<String>,
        variable_names: Vec<String>,
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
        plans.push(Plan {
            doc: doc_id,
            gen,
            model_name,
            parameters: ast_extract::extract_parameters_from_ast(ast),
            parameter_bounds: ast_extract::extract_parameter_bounds_from_ast(ast),
            inputs_with_defaults: ast_extract::extract_inputs_with_defaults_from_ast(ast),
            runtime_inputs: ast_extract::extract_input_names_from_ast(ast),
            variable_names: ast_extract::extract_variable_names_from_ast(ast),
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
                    for name in &plan.variable_names {
                        model.variables.insert(name.clone(), 0.0);
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
            let variables: HashMap<String, f64> = plan
                .variable_names
                .iter()
                .map(|n| (n.clone(), 0.0))
                .collect();
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
