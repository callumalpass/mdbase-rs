# Typed hosted-provider outcomes — Phase 4b

Status: implemented.

## Authority boundary

`CompiledCatalog` plans and evaluates canonical semantics over bounded exact inputs. It does not acquire storage authority. A hosted provider selects records/resources, owns snapshot consistency and cursor lifecycle, and persists a returned write set transactionally with its own CAS, fencing, conflict, cancellation, and retry policy.

Hosted plans never authorize a write merely because an operation is valid. `TypedHostedMutationPlan`, `TypedHostedResourceMutationPlan`, and `TypedHostedDefinitionPlan` return exact canonical evidence for the provider to persist or reject. The unprefixed `Hosted*Plan` names remain the unchanged legacy wire projections.

## Typed APIs

Connect should use:

- `read_record_typed` and `read_record_not_found_typed`
- `plan_hosted_mutation_typed`
- `execute_hosted_resource_read_typed`
- `plan_hosted_resource_mutation_typed`
- `plan_hosted_definition_operation_typed`
- `execute_hosted_validation_typed`
- `plan_hosted_canonical_view_typed`
- `finalize_hosted_query_page_typed`

`TypedHostedMutationPlan` exposes `CanonicalOperationOutcome`, `HostedRecordChange`, and `ChangeSet`. It contains no `OperationResult`. Each record change carries stable identity, exact before/after input documents, revisions, type sets, path/rename evidence, changed fields, and body-change evidence. The provider persists the returned exact after document directly; it does not hydrate a post-write semantic result.

For partial batches, mdbase advances its path-to-identity state strictly from valid typed `BatchResult.operations` in request order. Failed creates, updates, deletes, and renames neither alter identity state nor require snapshots. This supports generated create paths, operations depending on earlier successful items, repeated paths, and rename chains without treating failed request intent as applied evidence.

`TypedHostedResourceMutationPlan` and `TypedHostedDefinitionPlan` expose exact `ResourceChange` values and `ChangeSet`, including before/after revisions, and contain no compatibility result. Definition apply evidence is derived from the bounded before/after resource stages, not inferred by Connect from assessment or receipt JSON. Assessments return no changes.

Hosted query pages expose named `ProjectedValue` values, `QueryMetadata`, `CanonicalOperationOutcome`, and digest-bound `HostedQueryCursorState`. Cursor storage, expiry, encryption, consumption, conflict handling, and cancellation remain provider responsibilities. Existing plan fallback rules and all compiled budgets are unchanged.

## Compatibility edge

The legacy methods remain fully source-compatible wrappers. `HostedMutationPlan`, `HostedResourceMutationPlan`, `HostedDefinitionPlan`, and `HostedMutationChange` retain exactly their original public fields, so existing literals and exhaustive destructuring continue compiling. These legacy plans contain only the historical v0.3 result/write-set shape. Wrappers project the typed operation through `CanonicalOperationOutcome::to_v03` only after typed planning. Typed plans contain no redundant compatibility result. `CanonicalOperationOutcome::to_v03` remains the one public typed-to-v0.3 adapter.

Normal typed hosted modules do not call `recover_v03`/`from_v03` and architecture checks reject those calls or nested `OperationResult.result` inference. The canonical implementation edge temporarily adapts operation families whose implementation still emits the v0.3 envelope; hosts never see or invoke that edge.

## Remaining wire-only values

No public typed value model yet exists for these semantic payload families, so their `CanonicalOperationOutcome` uses explicitly named `WireOnlyOperationValue` variants while preserving typed validity and diagnostics:

- `Validation`
- `ViewResource` (list/execute/read/create/update/delete view source)
- `TypeResource` (list/read/create/update type)
- `TypePack` (assess/apply)
- `CollectionSetup` (assess/apply)

Record reads, queries, CRUD, rename, and batch outcomes are not wire-only. Resource and definition mutations additionally carry typed `ResourceChange` evidence, so Connect does not inspect wire-only payloads to decide what changed.
