//! Bounded resource-only reads for hosted authorities.

use std::fs;
use std::path::Component;

use serde_json::Value;

use crate::v03::OperationResult;
use crate::{Collection, SpecProfile};

use super::{CatalogError, CompiledCatalog};

const MAX_HOSTED_RESOURCE_DOCUMENTS: usize = 2_000;
const MAX_HOSTED_RESOURCE_BYTES: usize = 32 * 1024 * 1024;

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
    }
}
