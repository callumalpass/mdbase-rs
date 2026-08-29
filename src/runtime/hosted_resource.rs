//! Bounded resource-only reads for hosted authorities.

use std::fs;
use std::path::Component;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::v03::OperationResult;
use crate::{Collection, SpecProfile};

use super::{CatalogError, CompiledCatalog, ResolvedTypeResource};

const MAX_HOSTED_RESOURCE_DOCUMENTS: usize = 2_000;
const MAX_HOSTED_RESOURCE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostedResourceKind {
    Configuration,
    Lock,
    Contract,
    Schema,
    Type,
    View,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedResourceDocument {
    pub path: String,
    pub kind: HostedResourceKind,
    pub revision: String,
    pub document: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedResourceMutationPlan {
    pub result: OperationResult,
    pub documents: Vec<HostedResourceDocument>,
    pub types: Vec<ResolvedTypeResource>,
    pub contracts: Vec<crate::data_contracts::ResolvedRecordContract>,
}

/// Closed provider-neutral definition operation executed over bounded exact
/// resource documents. Record Markdown is deliberately absent from this seam.
pub enum HostedDefinitionOperation<'a> {
    AssessTypePack {
        provision: &'a crate::v03::TypePackProvision,
        options: &'a crate::v03::TypePackAssessmentOptions,
    },
    ApplyTypePack {
        provision: &'a crate::v03::TypePackProvision,
        options: &'a crate::v03::TypePackApplyOptions,
    },
    AssessCollectionSetup {
        setup: &'a crate::v03::CollectionSetup,
    },
    ApplyCollectionSetup {
        setup: &'a crate::v03::CollectionSetup,
        options: &'a crate::v03::CollectionSetupApplyOptions,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedDefinitionPlan {
    pub result: OperationResult,
    /// Complete post-operation resource stage. This remains bounded by the
    /// same count and byte limits as every other hosted resource operation.
    pub documents: Vec<(String, String)>,
    pub types: Vec<ResolvedTypeResource>,
    pub contracts: Vec<crate::data_contracts::ResolvedRecordContract>,
}

impl CompiledCatalog {
    /// Execute a closed resource-only operation without staging any records.
    pub fn execute_hosted_resource_read(
        &self,
        operation: &str,
        input: &Value,
        resources: &[(String, String)],
    ) -> Result<OperationResult, CatalogError> {
        if !matches!(operation, "read_type" | "list_views" | "read_view_source") {
            return Err(resource_error(
                "unsupported_hosted_resource_read",
                "The operation is not in the closed hosted resource-read set.",
            ));
        }
        if resources.len() > MAX_HOSTED_RESOURCE_DOCUMENTS {
            return Err(resource_error(
                "hosted_resource_count_budget_exceeded",
                "The hosted resource read exceeds its document-count budget.",
            ));
        }
        let bytes = resources.iter().try_fold(0_usize, |total, (_, document)| {
            total.checked_add(document.len())
        });
        if bytes.is_none_or(|bytes| bytes > MAX_HOSTED_RESOURCE_BYTES) {
            return Err(resource_error(
                "hosted_resource_byte_budget_exceeded",
                "The hosted resource read exceeds its exact-byte budget.",
            ));
        }
        let directory = tempfile::tempdir().map_err(resource_stage_error)?;
        for (path, document) in resources {
            let relative = std::path::Path::new(path);
            if relative.is_absolute()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(resource_error(
                    "invalid_resource_path",
                    "A hosted resource path escapes its disposable stage.",
                ));
            }
            let destination = directory.path().join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(resource_stage_error)?;
            }
            fs::write(destination, document).map_err(resource_stage_error)?;
        }
        let data_contracts = crate::data_contracts::DataContractRegistry::load_resolved(
            self.contracts.clone(),
            &self.collection.types,
        )
        .map_err(|error| resource_error(error.code, error.message))?;
        let collection = Collection {
            root: directory.path().to_path_buf(),
            spec_profile: SpecProfile::V03,
            settings: self.collection.settings.clone(),
            config_extensions: self.collection.config_extensions.clone(),
            types: self.collection.types.clone(),
            type_plans: self.collection.type_plans.clone(),
            type_warnings: self.collection.type_warnings.clone(),
            data_contracts,
            root_capability: Collection::capability_for_root(directory.path())
                .map_err(resource_stage_error)?,
        };
        let operations = collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile");
        Ok(match operation {
            "read_type" => operations.read_type(input),
            "list_views" => operations.list_views(input),
            "read_view_source" => operations.read_view_source(input),
            _ => unreachable!(),
        })
    }

    /// Execute one resource mutation in a disposable resource-only stage and
    /// return the complete bounded resource/catalog write set.
    pub fn plan_hosted_resource_mutation(
        &self,
        operation: &str,
        input: &Value,
        resources: &[HostedResourceDocument],
    ) -> Result<HostedResourceMutationPlan, CatalogError> {
        if !matches!(
            operation,
            "create_type"
                | "update_type"
                | "create_view_source"
                | "update_view_source"
                | "delete_view_source"
        ) {
            return Err(resource_error(
                "unsupported_hosted_resource_mutation",
                "The operation is not in the closed hosted resource-mutation set.",
            ));
        }
        let exact_resources = resources
            .iter()
            .map(|resource| (resource.path.clone(), resource.document.clone()))
            .collect::<Vec<_>>();
        let (directory, collection) = self.stage_resources(&exact_resources)?;
        let operations = collection
            .v03_operations()
            .expect("compiled catalogs always use the canonical profile");
        let result = match operation {
            "create_type" => operations.create_type(input),
            "update_type" => operations.update_type(input),
            "create_view_source" => operations.create_view_source(input),
            "update_view_source" => operations.update_view_source(input),
            "delete_view_source" => operations.delete_view_source(input),
            _ => unreachable!(),
        };
        if !result.valid {
            return Ok(HostedResourceMutationPlan {
                result,
                documents: Vec::new(),
                types: Vec::new(),
                contracts: Vec::new(),
            });
        }
        let reloaded = Collection::open(directory.path()).map_err(|error| {
            resource_error(
                "hosted_resource_reload_failed",
                format!("Mutated hosted resources could not be reopened: {error}"),
            )
        })?;
        let changed_path = result
            .result
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                resource_error(
                    "hosted_resource_plan_incomplete",
                    "The canonical resource mutation omitted its changed path.",
                )
            })?;
        let mut expected = resources
            .iter()
            .map(|resource| (resource.path.clone(), resource.kind))
            .collect::<std::collections::BTreeMap<_, _>>();
        if operation == "delete_view_source" {
            expected.remove(changed_path);
        } else {
            expected.insert(
                changed_path.to_string(),
                if operation.ends_with("type") {
                    HostedResourceKind::Type
                } else {
                    HostedResourceKind::View
                },
            );
        }
        let mut documents = expected
            .into_iter()
            .map(|(path, kind)| {
                let document = fs::read_to_string(directory.path().join(&path))
                    .map_err(resource_stage_error)?;
                Ok(HostedResourceDocument {
                    revision: crate::v03::revision(document.as_bytes()),
                    path,
                    kind,
                    document,
                })
            })
            .collect::<Result<Vec<_>, CatalogError>>()?;
        documents.sort_by(|left, right| left.path.cmp(&right.path));
        let report = crate::v03::inspect_collection(directory.path());
        if !report.valid {
            let diagnostic = report
                .diagnostics
                .into_iter()
                .find(|diagnostic| diagnostic.severity == "error")
                .unwrap_or_else(|| {
                    crate::diagnostic::Diagnostic::error(
                        "invalid_type_definition",
                        "The mutated hosted type registry is invalid.",
                        None,
                    )
                });
            return Err(resource_error(diagnostic.code, diagnostic.message));
        }
        let mut types = report
            .types
            .into_iter()
            .map(|type_file| {
                let revision = documents
                    .iter()
                    .find(|resource| resource.path == type_file.path)
                    .map(|resource| resource.revision.clone())
                    .ok_or_else(|| {
                        resource_error(
                            "hosted_resource_snapshot_incomplete",
                            format!(
                                "Type resource '{}' was omitted from the snapshot.",
                                type_file.path
                            ),
                        )
                    })?;
                Ok(ResolvedTypeResource {
                    path: type_file.path,
                    revision,
                    definition: type_file.frontmatter,
                    schema: type_file.schema,
                })
            })
            .collect::<Result<Vec<_>, CatalogError>>()?;
        types.sort_by(|left, right| left.path.cmp(&right.path));
        let mut contracts = reloaded
            .list_data_contracts()
            .into_iter()
            .filter_map(|definition| {
                let record_schema = definition.record_schema?;
                let implementations =
                    reloaded.get_data_contract_implementations(&definition.id, &definition.version);
                (!implementations.is_empty()).then(|| {
                    crate::data_contracts::ResolvedRecordContract {
                        id: definition.id,
                        version: definition.version,
                        digest: definition.digest,
                        record_schema,
                        binding_schema: definition.binding_schema,
                        implementations: implementations
                            .into_iter()
                            .map(|implementation| {
                                crate::data_contracts::ResolvedRecordContractImplementation {
                                    type_name: implementation.type_name,
                                    type_version: implementation.type_version,
                                    digest: implementation.implementation_digest,
                                    fields: implementation.fields,
                                    binding: implementation.binding,
                                    source_path: implementation.source_path,
                                }
                            })
                            .collect(),
                    }
                })
            })
            .collect::<Vec<_>>();
        contracts
            .sort_by(|left, right| (&left.id, &left.version).cmp(&(&right.id, &right.version)));
        Ok(HostedResourceMutationPlan {
            result,
            documents,
            types,
            contracts,
        })
    }

    /// Assess or apply a collection-definition operation without staging any
    /// record document. Apply operations return the complete bounded resource
    /// stage and the resulting canonical type/contract catalog for atomic host
    /// persistence.
    pub fn plan_hosted_definition_operation(
        &self,
        operation: HostedDefinitionOperation<'_>,
        resources: &[(String, String)],
    ) -> Result<HostedDefinitionPlan, CatalogError> {
        let (directory, collection) = self.stage_resources(resources)?;
        let (result, applied) = match operation {
            HostedDefinitionOperation::AssessTypePack { provision, options } => {
                (collection.assess_type_pack(provision, options), false)
            }
            HostedDefinitionOperation::ApplyTypePack { provision, options } => {
                (collection.apply_type_pack(provision, options), true)
            }
            HostedDefinitionOperation::AssessCollectionSetup { setup } => {
                (collection.assess_collection_setup(setup), false)
            }
            HostedDefinitionOperation::ApplyCollectionSetup { setup, options } => {
                (collection.apply_collection_setup(setup, options), true)
            }
        };
        if !applied || !result.valid {
            return Ok(HostedDefinitionPlan {
                result,
                documents: Vec::new(),
                types: Vec::new(),
                contracts: Vec::new(),
            });
        }
        let documents = load_definition_stage(directory.path())?;
        let (types, contracts) = resolve_definition_catalog(directory.path(), &documents)?;
        Ok(HostedDefinitionPlan {
            result,
            documents,
            types,
            contracts,
        })
    }

    fn stage_resources(
        &self,
        resources: &[(String, String)],
    ) -> Result<(tempfile::TempDir, Collection), CatalogError> {
        validate_resource_budget(resources)?;
        let directory = tempfile::tempdir().map_err(resource_stage_error)?;
        write_resources(directory.path(), resources)?;
        let data_contracts = crate::data_contracts::DataContractRegistry::load_resolved(
            self.contracts.clone(),
            &self.collection.types,
        )
        .map_err(|error| resource_error(error.code, error.message))?;
        let collection = Collection {
            root: directory.path().to_path_buf(),
            spec_profile: SpecProfile::V03,
            settings: self.collection.settings.clone(),
            config_extensions: self.collection.config_extensions.clone(),
            types: self.collection.types.clone(),
            type_plans: self.collection.type_plans.clone(),
            type_warnings: self.collection.type_warnings.clone(),
            data_contracts,
            root_capability: Collection::capability_for_root(directory.path())
                .map_err(resource_stage_error)?,
        };
        Ok((directory, collection))
    }
}

fn load_definition_stage(root: &std::path::Path) -> Result<Vec<(String, String)>, CatalogError> {
    let mut documents = Vec::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.map_err(|error| {
            resource_error(
                "hosted_resource_stage_failed",
                format!("The hosted definition stage could not be inspected: {error}"),
            )
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root).map_err(|error| {
            resource_error(
                "hosted_resource_stage_failed",
                format!("The hosted definition path could not be normalized: {error}"),
            )
        })?;
        if relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".mdbase")
        {
            continue;
        }
        let path = relative.to_str().ok_or_else(|| {
            resource_error(
                "invalid_resource_path",
                "A hosted definition path is not valid UTF-8.",
            )
        })?;
        let document = fs::read_to_string(entry.path()).map_err(resource_stage_error)?;
        documents.push((path.replace('\\', "/"), document));
    }
    documents.sort_by(|left, right| left.0.cmp(&right.0));
    validate_resource_budget(&documents)?;
    Ok(documents)
}

fn resolve_definition_catalog(
    root: &std::path::Path,
    documents: &[(String, String)],
) -> Result<
    (
        Vec<ResolvedTypeResource>,
        Vec<crate::data_contracts::ResolvedRecordContract>,
    ),
    CatalogError,
> {
    let collection = Collection::open(root).map_err(|error| {
        resource_error(
            "hosted_resource_reload_failed",
            format!("Mutated hosted definitions could not be reopened: {error}"),
        )
    })?;
    let report = crate::v03::inspect_collection(root);
    if !report.valid {
        let diagnostic = report
            .diagnostics
            .into_iter()
            .find(|diagnostic| diagnostic.severity == "error")
            .unwrap_or_else(|| {
                crate::diagnostic::Diagnostic::error(
                    "invalid_type_definition",
                    "The mutated hosted definition registry is invalid.",
                    None,
                )
            });
        return Err(resource_error(diagnostic.code, diagnostic.message));
    }
    let mut types = report
        .types
        .into_iter()
        .map(|type_file| {
            let document = documents
                .iter()
                .find(|(path, _)| path == &type_file.path)
                .map(|(_, document)| document)
                .ok_or_else(|| {
                    resource_error(
                        "hosted_resource_snapshot_incomplete",
                        format!(
                            "Type resource '{}' was omitted from the definition stage.",
                            type_file.path
                        ),
                    )
                })?;
            Ok(ResolvedTypeResource {
                path: type_file.path,
                revision: crate::v03::revision(document.as_bytes()),
                definition: type_file.frontmatter,
                schema: type_file.schema,
            })
        })
        .collect::<Result<Vec<_>, CatalogError>>()?;
    types.sort_by(|left, right| left.path.cmp(&right.path));
    let mut contracts = collection
        .list_data_contracts()
        .into_iter()
        .filter_map(|definition| {
            let record_schema = definition.record_schema?;
            let implementations =
                collection.get_data_contract_implementations(&definition.id, &definition.version);
            (!implementations.is_empty()).then(|| crate::data_contracts::ResolvedRecordContract {
                id: definition.id,
                version: definition.version,
                digest: definition.digest,
                record_schema,
                binding_schema: definition.binding_schema,
                implementations: implementations
                    .into_iter()
                    .map(|implementation| {
                        crate::data_contracts::ResolvedRecordContractImplementation {
                            type_name: implementation.type_name,
                            type_version: implementation.type_version,
                            digest: implementation.implementation_digest,
                            fields: implementation.fields,
                            binding: implementation.binding,
                            source_path: implementation.source_path,
                        }
                    })
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    contracts.sort_by(|left, right| (&left.id, &left.version).cmp(&(&right.id, &right.version)));
    Ok((types, contracts))
}

fn validate_resource_budget(resources: &[(String, String)]) -> Result<(), CatalogError> {
    if resources.len() > MAX_HOSTED_RESOURCE_DOCUMENTS {
        return Err(resource_error(
            "hosted_resource_count_budget_exceeded",
            "The hosted resource operation exceeds its document-count budget.",
        ));
    }
    let bytes = resources.iter().try_fold(0_usize, |total, (_, document)| {
        total.checked_add(document.len())
    });
    if bytes.is_none_or(|bytes| bytes > MAX_HOSTED_RESOURCE_BYTES) {
        return Err(resource_error(
            "hosted_resource_byte_budget_exceeded",
            "The hosted resource operation exceeds its exact-byte budget.",
        ));
    }
    Ok(())
}

fn write_resources(
    root: &std::path::Path,
    resources: &[(String, String)],
) -> Result<(), CatalogError> {
    for (path, document) in resources {
        let relative = std::path::Path::new(path);
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(resource_error(
                "invalid_resource_path",
                "A hosted resource path escapes its disposable stage.",
            ));
        }
        let destination = root.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(resource_stage_error)?;
        }
        fs::write(destination, document).map_err(resource_stage_error)?;
    }
    Ok(())
}

fn resource_error(code: impl Into<String>, message: impl Into<String>) -> CatalogError {
    CatalogError {
        code: code.into(),
        message: message.into(),
    }
}

fn resource_stage_error(error: std::io::Error) -> CatalogError {
    resource_error(
        "hosted_resource_stage_failed",
        format!("Hosted resources could not be staged: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::json;

    use super::*;
    use crate::runtime::{CatalogInput, ResolvedTypeResource};

    #[test]
    fn reads_types_and_views_without_record_documents() {
        let configuration = "spec_version: 0.3.0\nsettings:\n  types_folder: _types\n";
        let type_document = "---\nkind: mdbase.type\nname: task\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n";
        let type_definition = json!({
            "kind": "mdbase.type",
            "name": "task",
            "version": 1,
            "schema": {
                "dialect": "json-schema-2020-12",
                "value": {"type": "object"}
            }
        });
        let catalog = CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-1".to_string(),
            configuration_document: configuration.to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "type-1".to_string(),
                definition: type_definition,
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap();
        let resources = vec![
            ("mdbase.yaml".to_string(), configuration.to_string()),
            ("_types/task.md".to_string(), type_document.to_string()),
            (
                "views/open.md".to_string(),
                "---\ntype: view\nid: open.views\nversion: 1\nname: Open\nquery: {}\nviews:\n  - id: all\n    name: All\n---\n"
                    .to_string(),
            ),
        ];
        let type_result = catalog
            .execute_hosted_resource_read("read_type", &json!({"name": "task"}), &resources)
            .unwrap();
        assert!(type_result.valid);
        assert_eq!(type_result.result["document"], type_document);
        let view_result = catalog
            .execute_hosted_resource_read(
                "read_view_source",
                &json!({"path": "views/open.md"}),
                &resources,
            )
            .unwrap();
        assert!(view_result.valid);
        assert_eq!(view_result.result["path"], "views/open.md");
        let mutation_resources = resources
            .iter()
            .map(|(path, document)| HostedResourceDocument {
                path: path.clone(),
                kind: if path == "mdbase.yaml" {
                    HostedResourceKind::Configuration
                } else if path.starts_with("_types/") {
                    HostedResourceKind::Type
                } else {
                    HostedResourceKind::View
                },
                revision: crate::v03::revision(document.as_bytes()),
                document: document.clone(),
            })
            .collect::<Vec<_>>();

        let project_document = "---\nkind: mdbase.type\nname: project\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n";
        let type_mutation = catalog
            .plan_hosted_resource_mutation(
                "create_type",
                &json!({"document": project_document}),
                &mutation_resources,
            )
            .unwrap();
        assert!(type_mutation.result.valid);
        assert_eq!(type_mutation.types.len(), 2);
        assert!(type_mutation
            .documents
            .iter()
            .any(|resource| resource.path == "_types/project.md"));

        let view_document = "---\ntype: view\nid: recent.views\nversion: 1\nname: Recent\nquery: {}\nviews:\n  - id: all\n    name: All\n---\n";
        let view_mutation = catalog
            .plan_hosted_resource_mutation(
                "create_view_source",
                &json!({"path": "views/recent.md", "document": view_document}),
                &mutation_resources,
            )
            .unwrap();
        assert!(view_mutation.result.valid);
        assert!(view_mutation.documents.iter().any(|resource| {
            resource.path == "views/recent.md" && resource.kind == HostedResourceKind::View
        }));
    }

    #[test]
    fn definition_operations_stage_only_bounded_resources() {
        let configuration = "spec_version: 0.3.0\nsettings:\n  types_folder: _types\n";
        let existing_type = "---\nkind: mdbase.type\nname: task\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n";
        let project_type = "---\nkind: mdbase.type\nname: project\nversion: 1\nschema:\n  dialect: json-schema-2020-12\n  value: {type: object}\n---\n";
        let catalog = CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-1".to_string(),
            configuration_document: configuration.to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: crate::v03::revision(existing_type.as_bytes()),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "schema": {
                        "dialect": "json-schema-2020-12",
                        "value": {"type": "object"}
                    }
                }),
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap();
        let resources = vec![
            ("mdbase.yaml".to_string(), configuration.to_string()),
            ("_types/task.md".to_string(), existing_type.to_string()),
        ];
        let provision = crate::v03::TypePackProvision {
            manifest: json!({
                "kind": "mdbase.type-pack",
                "id": "dev.mdbase.hosted-test",
                "version": "1.0.0",
                "resources": [{
                    "kind": "type",
                    "mode": "managed",
                    "source": "types/project.md",
                    "target": "_types/project.md",
                    "digest": crate::v03::revision(project_type.as_bytes())
                }]
            }),
            resources: vec![crate::v03::TypePackResource {
                source: "types/project.md".to_string(),
                document: project_type.to_string(),
            }],
        };
        let assessment_options = crate::v03::TypePackAssessmentOptions {
            installed_by: "dev.mdbase.hosted-test".to_string(),
            adopt_resources: BTreeMap::new(),
            preserve_seed_targets: BTreeSet::new(),
            target_overrides: BTreeMap::new(),
            contract_setups: Vec::new(),
        };
        let assessment = catalog
            .plan_hosted_definition_operation(
                HostedDefinitionOperation::AssessTypePack {
                    provision: &provision,
                    options: &assessment_options,
                },
                &resources,
            )
            .unwrap();
        assert!(
            assessment.result.valid,
            "{:?}",
            assessment.result.diagnostics
        );
        assert!(assessment.documents.is_empty());
        let apply_options = crate::v03::TypePackApplyOptions {
            installed_by: assessment_options.installed_by.clone(),
            expected_assessment_digest: assessment.result.result["assessment_digest"]
                .as_str()
                .unwrap()
                .to_string(),
            allow_downgrade: false,
            adopt_resources: BTreeMap::new(),
            preserve_seed_targets: BTreeSet::new(),
            target_overrides: BTreeMap::new(),
            contract_setups: Vec::new(),
        };
        let applied = catalog
            .plan_hosted_definition_operation(
                HostedDefinitionOperation::ApplyTypePack {
                    provision: &provision,
                    options: &apply_options,
                },
                &resources,
            )
            .unwrap();
        assert!(applied.result.valid, "{:?}", applied.result.diagnostics);
        assert!(applied
            .documents
            .iter()
            .any(|(path, document)| path == "_types/project.md" && document == project_type));
        assert!(applied
            .documents
            .iter()
            .any(|(path, _)| path == "mdbase.lock.yaml"));
        assert_eq!(applied.types.len(), 2);
    }
}
