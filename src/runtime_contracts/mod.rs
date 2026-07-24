//! Pure mdbase Runtime Contracts profile 0.1 registry and preflight.
//!
//! This module validates and composes materialized or virtual contracts. It
//! deliberately has no action handlers and performs no I/O during dispatch;
//! embedding hosts remain responsible for final authorization and execution.

mod loader;
mod materialize;
mod model;
mod preflight;
mod registry;
mod schemas;

pub use model::{
    ComposeOptions, ContractDocument, ContractEntry, ContractKind, ContractOrigin, ContractSource,
    LoadOptions, PolicySelector, PreflightReport, RuntimeDiagnostic, RuntimeLoadResult,
    RuntimePackage, SourceKind, ValidationResult, WorkflowResolution, RUNTIME_PROFILE_VERSION,
};
pub use registry::RuntimeRegistry;

use serde_json::Value;

use crate::Collection;

/// Reusable validator and registry composer for Runtime Contracts profile 0.1.
pub struct RuntimeContracts {
    validators: schemas::CanonicalValidators,
}

impl RuntimeContracts {
    /// Compile the vendored canonical schemas once for repeated validation.
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            validators: schemas::CanonicalValidators::new()?,
        })
    }

    /// Discover materialized contracts using the collection's own scan scope.
    pub fn load_collection(&self, collection: &Collection) -> RuntimePackage {
        loader::load_collection(&self.validators, collection)
    }

    /// Compose materialized and virtual sources in canonical source order.
    pub fn compose(
        &self,
        sources: Vec<ContractSource>,
        options: &ComposeOptions,
    ) -> RuntimeRegistry {
        registry::compose(&self.validators, sources, options)
    }

    /// Load, compose, and preflight one collection. `runtime.policy` is read
    /// only as a local selector and never grants dispatch authority.
    pub fn load(
        &self,
        collection: &Collection,
        implicit_sources: Vec<ContractSource>,
        options: &LoadOptions,
    ) -> RuntimeLoadResult {
        let contracts = self.load_collection(collection);
        let (selected_policies, selection_diagnostics) =
            loader::selected_policies(collection, options);
        let mut sources = implicit_sources;
        sources.push(ContractSource::collection(contracts.records.clone()));
        let mut registry = self.compose(sources, &ComposeOptions { selected_policies });
        registry.diagnostics.extend(contracts.diagnostics.clone());
        registry.diagnostics.extend(selection_diagnostics);
        let preflight = self.preflight(&registry);
        RuntimeLoadResult {
            contracts,
            registry,
            preflight,
        }
    }

    pub fn validate_contract(&self, document: &ContractDocument) -> ValidationResult {
        self.validators.validate_contract(document)
    }

    pub fn preflight(&self, registry: &RuntimeRegistry) -> PreflightReport {
        preflight::preflight(registry)
    }

    pub fn validate_event(&self, registry: &RuntimeRegistry, envelope: &Value) -> ValidationResult {
        preflight::validate_event(&self.validators, registry, envelope)
    }

    pub fn validate_action_input(
        &self,
        registry: &RuntimeRegistry,
        action_id: &str,
        input: &Value,
    ) -> ValidationResult {
        preflight::validate_action_input(registry, action_id, input)
    }

    pub fn validate_action_output(
        &self,
        registry: &RuntimeRegistry,
        action_id: &str,
        output: &Value,
    ) -> ValidationResult {
        preflight::validate_action_output(registry, action_id, output)
    }

    /// Validate input against the action contract snapshot pinned into an
    /// admitted execution plan.
    pub fn validate_pinned_action_input(&self, action: &Value, input: &Value) -> ValidationResult {
        let document = ContractDocument::virtual_contract(action.clone());
        let (mut validation, embedded) = self.validators.prepare_contract(&document);
        if validation.valid {
            validation
                .diagnostics
                .extend(embedded.action_input.map_or_else(
                    || {
                        vec![RuntimeDiagnostic::error(
                            "invalid_embedded_schema",
                            "Pinned action has no compiled input schema.",
                        )]
                    },
                    |schema| {
                        schemas::validate_compiled(&schema, input, "<pinned-action.input>")
                            .diagnostics
                    },
                ));
        }
        ValidationResult::new(validation.diagnostics)
    }

    /// Validate output against the action contract snapshot pinned into an
    /// admitted execution plan.
    pub fn validate_pinned_action_output(
        &self,
        action: &Value,
        output: &Value,
    ) -> ValidationResult {
        let document = ContractDocument::virtual_contract(action.clone());
        let (mut validation, embedded) = self.validators.prepare_contract(&document);
        if validation.valid
            && action
                .pointer("/schemas/output")
                .is_some_and(|value| !value.is_null())
        {
            validation
                .diagnostics
                .extend(embedded.action_output.map_or_else(
                    || {
                        vec![RuntimeDiagnostic::error(
                            "invalid_embedded_schema",
                            "Pinned action has no compiled output schema.",
                        )]
                    },
                    |schema| {
                        schemas::validate_compiled(&schema, output, "<pinned-action.output>")
                            .diagnostics
                    },
                ));
        }
        ValidationResult::new(validation.diagnostics)
    }

    /// Apply the currently selected policy to a previously pinned action
    /// definition. Registry changes can tighten authorization without
    /// replacing the execution plan's action shape.
    pub fn preflight_pinned_action(
        &self,
        registry: &RuntimeRegistry,
        action: &Value,
        dispatch_context: &Value,
    ) -> ValidationResult {
        preflight::preflight_action_contract(registry, action, dispatch_context)
    }

    /// Check registry policy for a prospective dispatch. Successful preflight
    /// is advisory; an embedding host must independently authorize the current
    /// actor, resource, grant, and action immediately before execution.
    pub fn preflight_action(
        &self,
        registry: &RuntimeRegistry,
        action_id: &str,
        dispatch_context: &Value,
    ) -> ValidationResult {
        preflight::preflight_action(registry, action_id, dispatch_context)
    }

    /// Render any valid materialized or virtual registry contract as Markdown.
    pub fn materialize_contract(
        &self,
        contract: &Value,
        body: Option<&str>,
    ) -> Result<String, Vec<RuntimeDiagnostic>> {
        let document = ContractDocument::virtual_contract(contract.clone());
        let validation = self.validate_contract(&document);
        if !validation.valid {
            return Err(validation.diagnostics);
        }
        materialize::contract_markdown(contract, body).map_err(|diagnostic| vec![*diagnostic])
    }
}
