use mdbase::runtime::{
    CanonicalOperationOutcome, CanonicalRecordInput, ChangeSet, CompiledCatalog,
    HostedCanonicalViewPlanningTyped, HostedDefinitionOperation, HostedDefinitionPlan,
    HostedMutationChange, HostedMutationPlan, HostedMutationRequest, HostedNamedProjectedValue,
    HostedQueryCursorState, HostedQueryPage, HostedQueryPageInput, HostedQueryPlan,
    HostedResourceDocument, HostedResourceMutationPlan, HostedValidationPlan,
    TypedHostedDefinitionPlan, TypedHostedMutationPlan, TypedHostedResourceMutationPlan,
};
use serde_json::Value;

#[allow(dead_code, deprecated, clippy::too_many_arguments)]
fn connect_api_compile_probe(
    catalog: &CompiledCatalog,
    input: &Value,
    record: &CanonicalRecordInput,
    mutation: &HostedMutationRequest,
    query: &HostedQueryPlan,
    validation: &HostedValidationPlan,
    resources: &[(String, String)],
    resource_documents: &[HostedResourceDocument],
    definition: HostedDefinitionOperation<'_>,
) {
    let read: Result<CanonicalOperationOutcome, _> = catalog.read_record_typed(input, record);
    let missing: Result<CanonicalOperationOutcome, _> = catalog.read_record_not_found_typed(input);
    let mutation: Result<TypedHostedMutationPlan, _> = catalog.plan_hosted_mutation_typed(mutation);
    let resource_read: Result<CanonicalOperationOutcome, _> =
        catalog.execute_hosted_resource_read_typed("read_type", input, resources);
    let resource_mutation: Result<TypedHostedResourceMutationPlan, _> =
        catalog.plan_hosted_resource_mutation_typed("create_type", input, resource_documents);
    let definition: Result<TypedHostedDefinitionPlan, _> =
        catalog.plan_hosted_definition_operation_typed(definition, resources);
    let validation: Result<CanonicalOperationOutcome, _> =
        catalog.execute_hosted_validation_typed(validation, std::slice::from_ref(record));
    let view: Result<HostedCanonicalViewPlanningTyped, _> =
        catalog.plan_hosted_canonical_view_typed(input, record, None, &[]);
    let page: Result<HostedQueryPage, _> = catalog.finalize_hosted_query_page_typed(
        query,
        HostedQueryPageInput {
            records: Vec::<HostedNamedProjectedValue>::new(),
            total_count: 0,
            has_more: false,
            meta: mdbase::api::QueryMetadata::new(serde_json::json!({})),
            cursor: None::<HostedQueryCursorState>,
            diagnostics: Vec::new(),
        },
    );

    if let Ok(plan) = mutation {
        let _: &CanonicalOperationOutcome = &plan.operation;
        let _: &ChangeSet = &plan.change_set;
    }
    let _ = (
        read,
        missing,
        resource_read,
        resource_mutation,
        definition,
        validation,
        view,
        page,
    );
}

#[allow(dead_code)]
fn legacy_plan_literals_and_exhaustive_destructuring_compile(
    result: mdbase::v03::OperationResult,
    resource_documents: Vec<HostedResourceDocument>,
    types: Vec<mdbase::runtime::ResolvedTypeResource>,
    contracts: Vec<mdbase::runtime::ResolvedRecordContract>,
) {
    let mutation_change = HostedMutationChange {
        stable_id: "record-1".to_string(),
        before_path: None,
        record: None,
    };
    let HostedMutationChange {
        stable_id: _,
        before_path: _,
        record: _,
    } = mutation_change.clone();
    let mutation = HostedMutationPlan {
        result: result.clone(),
        primary_stable_id: "record-1".to_string(),
        changes: vec![mutation_change],
    };
    let HostedMutationPlan {
        result: _,
        primary_stable_id: _,
        changes: _,
    } = mutation;

    let resource = HostedResourceMutationPlan {
        result: result.clone(),
        documents: resource_documents,
        types: types.clone(),
        contracts: contracts.clone(),
    };
    let HostedResourceMutationPlan {
        result: _,
        documents: _,
        types: _,
        contracts: _,
    } = resource;

    let definition = HostedDefinitionPlan {
        result,
        documents: Vec::new(),
        types,
        contracts,
    };
    let HostedDefinitionPlan {
        result: _,
        documents: _,
        types: _,
        contracts: _,
    } = definition;
}

#[test]
fn typed_hosted_api_is_publicly_nameable() {
    assert!(std::any::type_name::<HostedQueryCursorState>().contains("HostedQueryCursorState"));
}
