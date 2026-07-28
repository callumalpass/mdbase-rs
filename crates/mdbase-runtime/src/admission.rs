use mdbase_interop::{
    contract_digest, validate_action_provider_declaration, validate_event,
    validate_event_source_declaration, ActionProviderDeclaration, CloudEvent, ContractRequirement,
    EventSourceDeclaration, ExactContractReference, ImplementationIdentity, ProviderSelector,
};
use semver::{Version, VersionReq};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{RuntimeError, RuntimeResult};

/// Verified contracts, live interoperability declarations, and portable
/// runtime records used to admit events. Constructing a catalog is passive:
/// it never registers executable handlers or grants authority.
#[derive(Debug, Clone)]
pub struct AdmissionCatalog {
    contracts: Vec<ResolvedContract>,
    event_sources: Vec<EventSourceDeclaration>,
    action_providers: Vec<ActionProviderDeclaration>,
    workflows: Vec<Value>,
    policy: Value,
    revision: String,
}

#[derive(Debug, Clone)]
struct ResolvedContract {
    reference: ExactContractReference,
    artifact: Value,
}

#[derive(Debug, Clone)]
pub(crate) struct AdmittedWorkflow {
    pub workflow: Value,
    pub trigger: Value,
    pub workflow_revision: String,
    pub policy_id: String,
    pub policy_revision: String,
    pub event_contract: ExactContractReference,
    pub event_source: ImplementationIdentity,
    pub source_declaration_digest: String,
    pub steps: Vec<AdmittedAction>,
}

#[derive(Debug, Clone)]
pub(crate) struct AdmittedAction {
    pub id: String,
    pub contract: ExactContractReference,
    pub artifact: Value,
    pub provider: ImplementationIdentity,
    pub provider_declaration_digest: String,
    pub handler_id: String,
    pub idempotent: bool,
    pub cooperative_cancellation: bool,
}

impl AdmissionCatalog {
    pub fn new(
        contract_artifacts: Vec<Value>,
        event_sources: Vec<Value>,
        action_providers: Vec<Value>,
        workflows: Vec<Value>,
        policy: Value,
    ) -> RuntimeResult<Self> {
        let contracts = resolve_contracts(contract_artifacts)?;
        let event_sources = event_sources
            .into_iter()
            .map(|value| {
                let declaration = validate_event_source_declaration(&value)
                    .map_err(|issues| interop_error("invalid_event_source", issues))?;
                verify_declaration_digest(&value, "declaration_digest")?;
                Ok(declaration)
            })
            .collect::<RuntimeResult<Vec<_>>>()?;
        let action_providers = action_providers
            .into_iter()
            .map(|value| {
                let declaration = validate_action_provider_declaration(&value)
                    .map_err(|issues| interop_error("invalid_action_provider", issues))?;
                verify_declaration_digest(&value, "declaration_digest")?;
                Ok(declaration)
            })
            .collect::<RuntimeResult<Vec<_>>>()?;

        validate_declaration_contracts(&contracts, &event_sources, &action_providers)?;
        validate_runtime_records(&workflows, &policy)?;

        let revision = canonical_digest(&serde_json::json!({
            "contracts": contracts.iter().map(|entry| &entry.reference).collect::<Vec<_>>(),
            "event_sources": &event_sources,
            "action_providers": &action_providers,
            "workflows": &workflows,
            "policy": &policy,
        }))?;
        Ok(Self {
            contracts,
            event_sources,
            action_providers,
            workflows,
            policy,
            revision,
        })
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn policy(&self) -> &Value {
        &self.policy
    }

    pub fn workflows(&self) -> &[Value] {
        &self.workflows
    }

    pub(crate) fn admit_event(&self, event: &Value) -> RuntimeResult<Vec<AdmittedWorkflow>> {
        let cloud_event: CloudEvent = serde_json::from_value(event.clone()).map_err(|error| {
            RuntimeError::diagnostic(
                "invalid_runtime_event",
                format!("Event is not a structured mdbase CloudEvent: {error}"),
            )
        })?;
        let event_contract = self
            .contracts
            .iter()
            .find(|candidate| {
                candidate.reference.id == cloud_event.event_type
                    && candidate.reference.version == cloud_event.mdbasecontractversion
                    && candidate.reference.digest == cloud_event.mdbasecontractdigest
            })
            .ok_or_else(|| {
                RuntimeError::diagnostic(
                    "unknown_contract",
                    format!(
                        "Event {} references unavailable contract {} {} ({}).",
                        cloud_event.id,
                        cloud_event.event_type,
                        cloud_event.mdbasecontractversion,
                        cloud_event.mdbasecontractdigest
                    ),
                )
            })?;
        validate_event(&event_contract.artifact, event)
            .map_err(|issues| interop_error("invalid_runtime_event", issues))?;

        let source_identity = event_identity(&cloud_event);
        let source = self
            .event_sources
            .iter()
            .find(|declaration| {
                declaration.source == source_identity
                    && declaration
                        .contracts
                        .iter()
                        .any(|binding| binding.resolved == event_contract.reference)
            })
            .ok_or_else(|| {
                RuntimeError::diagnostic(
                    "event_source_unavailable",
                    format!(
                        "Event {} does not identify a verified source for {} {}.",
                        cloud_event.id,
                        event_contract.reference.id,
                        event_contract.reference.version
                    ),
                )
            })?;

        let mut admitted = Vec::new();
        for workflow in self
            .workflows
            .iter()
            .filter(|workflow| workflow.get("enabled") == Some(&Value::Bool(true)))
        {
            for trigger in workflow
                .get("triggers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let requirement = requirement(trigger.get("event"), "workflow trigger")?;
                if requirement.id != event_contract.reference.id
                    || !requirement_matches(&requirement, &event_contract.reference)?
                {
                    continue;
                }
                let steps = workflow
                    .get("steps")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|step| self.admit_step(step))
                    .collect::<RuntimeResult<Vec<_>>>()?;
                authorize_requirements(workflow, &steps, &self.policy)?;
                admitted.push(AdmittedWorkflow {
                    workflow: workflow.clone(),
                    trigger: trigger.clone(),
                    workflow_revision: canonical_digest(workflow)?,
                    policy_id: required_string(&self.policy, "id")?.to_string(),
                    policy_revision: canonical_digest(&self.policy)?,
                    event_contract: event_contract.reference.clone(),
                    event_source: source_identity.clone(),
                    source_declaration_digest: source.declaration_digest.clone(),
                    steps,
                });
            }
        }
        Ok(admitted)
    }

    pub(crate) fn selected_executor(&self, workflow_id: &str) -> Option<String> {
        self.policy
            .pointer(&format!(
                "/executors/workflows/{}",
                workflow_id.replace('~', "~0").replace('/', "~1")
            ))
            .and_then(Value::as_str)
            .or_else(|| {
                self.policy
                    .pointer("/executors/default")
                    .and_then(Value::as_str)
            })
            .map(str::to_string)
    }

    fn admit_step(&self, step: &Value) -> RuntimeResult<AdmittedAction> {
        let id = required_string(step, "id")?.to_string();
        let requirement = requirement(step.get("action"), "workflow step")?;
        let contract = resolve_requirement(&self.contracts, &requirement, "action")?;
        let selector = step
            .get("provider")
            .map(provider_selector)
            .transpose()?
            .or(self.policy_selector(&requirement)?);
        let mut candidates = self
            .action_providers
            .iter()
            .flat_map(|declaration| {
                declaration
                    .handlers
                    .iter()
                    .filter(|handler| handler.resolved == contract.reference)
                    .map(move |handler| (declaration, handler))
            })
            .filter(|(declaration, _)| {
                selector
                    .as_ref()
                    .is_none_or(|selector| matches_selector(&declaration.provider, selector))
            });
        let selected = candidates.next().ok_or_else(|| {
            RuntimeError::diagnostic(
                if selector.is_some() {
                    "requested_provider_unavailable"
                } else {
                    "no_provider"
                },
                format!(
                    "No eligible provider implements {} {}.",
                    contract.reference.id, contract.reference.version
                ),
            )
        })?;
        if candidates.next().is_some() {
            return Err(RuntimeError::diagnostic(
                "ambiguous_provider",
                format!(
                    "{} {} has multiple eligible providers; select one explicitly.",
                    contract.reference.id, contract.reference.version
                ),
            ));
        }
        let (declaration, handler) = selected;
        Ok(AdmittedAction {
            id,
            contract: contract.reference.clone(),
            artifact: contract.artifact.clone(),
            provider: declaration.provider.clone(),
            provider_declaration_digest: declaration.declaration_digest.clone(),
            handler_id: handler.handler_id.clone(),
            idempotent: handler
                .idempotency
                .as_ref()
                .is_some_and(|value| value.mode == "request"),
            cooperative_cancellation: handler.cancellation.as_deref() == Some("cooperative"),
        })
    }

    fn policy_selector(
        &self,
        action: &ContractRequirement,
    ) -> RuntimeResult<Option<ProviderSelector>> {
        let matches = self
            .policy
            .get("provider_selections")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|selection| {
                selection
                    .get("contract")
                    .and_then(|value| {
                        serde_json::from_value::<ContractRequirement>(value.clone()).ok()
                    })
                    .is_some_and(|candidate| {
                        candidate.id == action.id
                            && requirements_overlap(&self.contracts, &candidate, action)
                    })
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(RuntimeError::diagnostic(
                "ambiguous_provider_policy",
                format!(
                    "Runtime policy has multiple provider selections for {}.",
                    action.id
                ),
            ));
        }
        matches
            .first()
            .and_then(|selection| selection.get("selector"))
            .map(provider_selector)
            .transpose()
    }
}

fn resolve_contracts(values: Vec<Value>) -> RuntimeResult<Vec<ResolvedContract>> {
    let mut contracts = Vec::new();
    for artifact in values {
        mdbase_interop::validate_contract_artifact(&artifact)
            .map_err(|issues| interop_error("invalid_contract", issues))?;
        let reference = ExactContractReference {
            id: required_string(&artifact, "id")?.to_string(),
            version: required_string(&artifact, "version")?.to_string(),
            digest: contract_digest(&artifact)
                .map_err(|message| RuntimeError::diagnostic("invalid_contract", message))?,
        };
        if let Some(existing) = contracts.iter().find(|candidate: &&ResolvedContract| {
            candidate.reference.id == reference.id
                && candidate.reference.version == reference.version
        }) {
            if existing.reference.digest != reference.digest {
                return Err(RuntimeError::diagnostic(
                    "contract_digest_conflict",
                    format!(
                        "{} {} has conflicting digests.",
                        reference.id, reference.version
                    ),
                ));
            }
            continue;
        }
        contracts.push(ResolvedContract {
            reference,
            artifact,
        });
    }
    Ok(contracts)
}

fn validate_declaration_contracts(
    contracts: &[ResolvedContract],
    event_sources: &[EventSourceDeclaration],
    action_providers: &[ActionProviderDeclaration],
) -> RuntimeResult<()> {
    for (requirement, resolved, expected) in event_sources
        .iter()
        .flat_map(|declaration| declaration.contracts.iter())
        .map(|binding| (&binding.requirement, &binding.resolved, "event"))
        .chain(
            action_providers
                .iter()
                .flat_map(|declaration| declaration.handlers.iter())
                .map(|handler| (&handler.requirement, &handler.resolved, "action")),
        )
    {
        if !requirement_matches(requirement, resolved)?
            || contracts.iter().all(|candidate| {
                &candidate.reference != resolved
                    || candidate
                        .artifact
                        .get("contract_type")
                        .and_then(Value::as_str)
                        != Some(expected)
            })
        {
            return Err(RuntimeError::diagnostic(
                "invalid_implementation_declaration",
                format!(
                    "Declaration does not resolve {} {} to a registered {expected} contract.",
                    resolved.id, resolved.version
                ),
            ));
        }
    }
    Ok(())
}

fn validate_runtime_records(workflows: &[Value], policy: &Value) -> RuntimeResult<()> {
    crate::schemas::validate_runtime_record(policy)?;
    if policy.get("enabled") != Some(&Value::Bool(true)) {
        return Err(RuntimeError::diagnostic(
            "runtime_policy_disabled",
            "Admission requires one verified, enabled runtime_policy record.",
        ));
    }
    for workflow in workflows {
        crate::schemas::validate_runtime_record(workflow)?;
        assert_unique_ids(workflow, "triggers")?;
        assert_unique_ids(workflow, "steps")?;
    }
    Ok(())
}

fn assert_unique_ids(workflow: &Value, field: &str) -> RuntimeResult<()> {
    let mut ids = std::collections::BTreeSet::new();
    for value in workflow
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = required_string(value, "id")?;
        if !ids.insert(id) {
            return Err(RuntimeError::diagnostic(
                "duplicate_workflow_id",
                format!("Workflow repeats {field} ID {id}."),
            ));
        }
    }
    Ok(())
}

fn resolve_requirement<'a>(
    contracts: &'a [ResolvedContract],
    requirement: &ContractRequirement,
    contract_type: &str,
) -> RuntimeResult<&'a ResolvedContract> {
    let range = VersionReq::parse(&requirement.version).map_err(|error| {
        RuntimeError::diagnostic(
            "invalid_contract_requirement",
            format!(
                "{} has invalid SemVer requirement {}: {error}",
                requirement.id, requirement.version
            ),
        )
    })?;
    contracts
        .iter()
        .filter(|candidate| {
            candidate.reference.id == requirement.id
                && candidate
                    .artifact
                    .get("contract_type")
                    .and_then(Value::as_str)
                    == Some(contract_type)
                && requirement
                    .digest
                    .as_ref()
                    .is_none_or(|digest| digest == &candidate.reference.digest)
        })
        .filter_map(|candidate| {
            Version::parse(&candidate.reference.version)
                .ok()
                .filter(|version| range.matches(version))
                .map(|version| (version, candidate))
        })
        .max_by(|(left, _), (right, _)| left.cmp(right))
        .map(|(_, candidate)| candidate)
        .ok_or_else(|| {
            RuntimeError::diagnostic(
                "unknown_contract",
                format!(
                    "No {contract_type} contract {} satisfies {}.",
                    requirement.id, requirement.version
                ),
            )
        })
}

fn requirement(value: Option<&Value>, label: &str) -> RuntimeResult<ContractRequirement> {
    serde_json::from_value(value.cloned().unwrap_or(Value::Null)).map_err(|error| {
        RuntimeError::diagnostic(
            "invalid_contract_requirement",
            format!("{label} has an invalid contract requirement: {error}"),
        )
    })
}

fn requirement_matches(
    requirement: &ContractRequirement,
    exact: &ExactContractReference,
) -> RuntimeResult<bool> {
    if requirement.id != exact.id
        || requirement
            .digest
            .as_ref()
            .is_some_and(|digest| digest != &exact.digest)
    {
        return Ok(false);
    }
    let range = VersionReq::parse(&requirement.version).map_err(|error| {
        RuntimeError::diagnostic(
            "invalid_contract_requirement",
            format!(
                "{} has invalid SemVer requirement {}: {error}",
                requirement.id, requirement.version
            ),
        )
    })?;
    let version = Version::parse(&exact.version).map_err(|error| {
        RuntimeError::diagnostic(
            "invalid_contract",
            format!("Resolved version {} is invalid: {error}", exact.version),
        )
    })?;
    Ok(range.matches(&version))
}

fn requirements_overlap(
    contracts: &[ResolvedContract],
    left: &ContractRequirement,
    right: &ContractRequirement,
) -> bool {
    contracts.iter().any(|contract| {
        contract.reference.id == left.id
            && requirement_matches(left, &contract.reference).unwrap_or(false)
            && requirement_matches(right, &contract.reference).unwrap_or(false)
    })
}

fn provider_selector(value: &Value) -> RuntimeResult<ProviderSelector> {
    serde_json::from_value(value.clone()).map_err(|error| {
        RuntimeError::diagnostic(
            "invalid_provider_selector",
            format!("Provider selector is invalid: {error}"),
        )
    })
}

fn matches_selector(identity: &ImplementationIdentity, selector: &ProviderSelector) -> bool {
    selector
        .application
        .as_ref()
        .is_none_or(|value| value == &identity.application)
        && selector
            .implementation
            .as_ref()
            .is_none_or(|value| value == &identity.implementation)
        && selector
            .instance_id
            .as_ref()
            .is_none_or(|value| identity.instance_id.as_ref() == Some(value))
}

fn authorize_requirements(
    workflow: &Value,
    steps: &[AdmittedAction],
    policy: &Value,
) -> RuntimeResult<()> {
    let workflow_capabilities = workflow
        .pointer("/requires/capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    for capability in workflow_capabilities {
        authorize_capability(capability, None, policy)?;
    }
    for (step, admitted) in workflow
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .zip(steps)
    {
        for capability in step
            .pointer("/requires/capabilities")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            authorize_capability(capability, Some(admitted), policy)?;
        }
    }
    Ok(())
}

fn authorize_capability(
    capability: &str,
    action: Option<&AdmittedAction>,
    policy: &Value,
) -> RuntimeResult<()> {
    let matching = policy
        .get("grants")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|grant| grant.get("capability").and_then(Value::as_str) == Some(capability))
        .filter(|grant| {
            grant
                .get("actions")
                .and_then(Value::as_array)
                .is_none_or(|requirements| {
                    action.is_some_and(|action| {
                        requirements.iter().any(|requirement_value| {
                            serde_json::from_value::<ContractRequirement>(requirement_value.clone())
                                .ok()
                                .is_some_and(|requirement| {
                                    requirement_matches(&requirement, &action.contract)
                                        .unwrap_or(false)
                                })
                        })
                    })
                })
        })
        .filter(|grant| {
            grant
                .get("providers")
                .and_then(Value::as_array)
                .is_none_or(|selectors| {
                    action.is_some_and(|action| {
                        selectors.iter().any(|selector| {
                            provider_selector(selector)
                                .is_ok_and(|selector| matches_selector(&action.provider, &selector))
                        })
                    })
                })
        })
        .collect::<Vec<_>>();
    let allowed = matching
        .iter()
        .any(|grant| grant.get("mode").and_then(Value::as_str) == Some("allow"))
        && !matching
            .iter()
            .any(|grant| grant.get("mode").and_then(Value::as_str) == Some("deny"));
    if allowed {
        Ok(())
    } else {
        Err(RuntimeError::diagnostic(
            "capability_denied",
            format!("Runtime policy does not allow capability {capability}."),
        ))
    }
}

fn event_identity(event: &CloudEvent) -> ImplementationIdentity {
    ImplementationIdentity {
        application: event.mdbaseapplication.clone(),
        implementation: event.mdbaseimplementation.clone(),
        version: event.mdbaseimplementationversion.clone(),
        instance_id: event.mdbaseinstanceid.clone(),
    }
}

fn verify_declaration_digest(value: &Value, field: &str) -> RuntimeResult<()> {
    let expected = required_string(value, field)?;
    let mut portable = value.clone();
    portable
        .as_object_mut()
        .expect("validated declaration is an object")
        .remove(field);
    let actual = canonical_digest(&portable)?;
    if actual == expected {
        Ok(())
    } else {
        Err(RuntimeError::diagnostic(
            "declaration_digest_conflict",
            format!("Declaration digest {expected} does not match canonical content {actual}."),
        ))
    }
}

/// RFC 8785/JCS SHA-256 digest used by portable runtime records and
/// interoperability declarations.
pub fn canonical_digest(value: &Value) -> RuntimeResult<String> {
    let canonical = serde_jcs::to_vec(value).map_err(|error| {
        RuntimeError::diagnostic(
            "canonicalization_failed",
            format!("Could not canonicalize portable runtime value: {error}"),
        )
    })?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn required_string<'a>(value: &'a Value, field: &str) -> RuntimeResult<&'a str> {
    value.get(field).and_then(Value::as_str).ok_or_else(|| {
        RuntimeError::diagnostic(
            "invalid_runtime_value",
            format!("Runtime value requires string {field}."),
        )
    })
}

fn interop_error(code: &str, issues: Vec<mdbase_interop::ValidationIssue>) -> RuntimeError {
    RuntimeError::diagnostic(
        code,
        issues
            .into_iter()
            .map(|issue| format!("{} {}", issue.instance_path, issue.message))
            .collect::<Vec<_>>()
            .join("; "),
    )
}
