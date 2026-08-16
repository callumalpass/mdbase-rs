//! Provider-neutral canonical saved-view planning for hosted authorities.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::v03::{Diagnostic, OperationResult};

use super::{CanonicalRecordInput, CatalogError, CompiledCatalog, HostedQueryPlan};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostedCanonicalViewPlan {
    pub query: HostedQueryPlan,
    pub view_path: String,
    pub view_id: String,
    pub view_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_revision: Option<String>,
    pub invocation_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum HostedCanonicalViewPlanning {
    Planned { plan: Box<HostedCanonicalViewPlan> },
    Invalid { result: OperationResult },
}

impl CompiledCatalog {
    /// Compile one exact canonical saved-view record into the same closed query
    /// plan used by direct hosted queries. The view and optional `this` record
    /// are point inputs; no collection enumeration occurs in this seam.
    pub fn plan_hosted_canonical_view(
        &self,
        input: &Value,
        view_record: &CanonicalRecordInput,
        context_record: Option<&CanonicalRecordInput>,
    ) -> Result<HostedCanonicalViewPlanning, CatalogError> {
        let requested_path = input
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if requested_path != view_record.path {
            return Ok(invalid(
                "view_not_found",
                "The requested saved view does not match the supplied exact record.",
                requested_path,
            ));
        }
        if requested_path.ends_with(".base") {
            return Ok(invalid(
                "unsupported_hosted_view_format",
                "Obsidian Base execution requires the dedicated hosted Base planner.",
                requested_path,
            ));
        }
        let classified_view = self.classify_record(view_record)?;
        let document = Value::Object(classified_view.frontmatter.clone());
        let prepared = match crate::views::prepare_hosted_canonical_view(&document, input) {
            Ok(prepared) => prepared,
            Err(result) => return Ok(HostedCanonicalViewPlanning::Invalid { result }),
        };
        let context = match prepared.context_path.as_deref() {
            None => None,
            Some(path) if path == view_record.path => Some(classified_view.clone()),
            Some(path) => {
                let Some(record) = context_record.filter(|record| record.path == path) else {
                    return Ok(invalid(
                        "context_not_found",
                        format!("Query context record '{path}' was not found."),
                        path,
                    ));
                };
                Some(self.classify_record(record)?)
            }
        };
        if let Err(result) = crate::views::verify_canonical_view_context(
            &prepared,
            context.as_ref().map(|context| context.types.as_slice()),
        ) {
            return Ok(HostedCanonicalViewPlanning::Invalid { result });
        }
        let query = match self.compile_hosted_query(&prepared.query) {
            Ok(query) => query,
            Err(error) => return Ok(invalid(&error.code, error.message, &prepared.view_path)),
        };
        let context_revision = context.as_ref().map(|context| context.revision.clone());
        let digest_input = json!({
            "schema": "mdbase.hosted-canonical-view-invocation.v1",
            "view_path": prepared.view_path,
            "view_id": prepared.view_id,
            "view_revision": classified_view.revision,
            "context_path": prepared.context_path,
            "context_revision": context_revision,
            "query_plan_digest": query.plan_digest,
        });
        let canonical = serde_jcs::to_vec(&digest_input).map_err(|error| CatalogError {
            code: "hosted_view_plan_failed".to_string(),
            message: format!("Saved-view invocation could not be canonicalized: {error}"),
        })?;
        let plan = HostedCanonicalViewPlan {
            query,
            view_path: prepared.view_path,
            view_id: prepared.view_id,
            view_revision: classified_view.revision,
            context_path: prepared.context_path,
            context_revision,
            invocation_digest: format!("sha256:{:x}", Sha256::digest(canonical)),
        };
        Ok(HostedCanonicalViewPlanning::Planned {
            plan: Box::new(plan),
        })
    }
}

fn invalid(
    code: impl Into<String>,
    message: impl Into<String>,
    field: impl Into<String>,
) -> HostedCanonicalViewPlanning {
    HostedCanonicalViewPlanning::Invalid {
        result: OperationResult {
            valid: false,
            result: json!({}),
            diagnostics: vec![Diagnostic::error(code, message, Some(field.into()))],
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::{CatalogInput, ResolvedTypeResource};

    fn catalog() -> CompiledCatalog {
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "semantic:test".to_string(),
            configuration_document: "spec_version: 0.3.0\n".to_string(),
            types: vec![ResolvedTypeResource {
                path: "_types/task.md".to_string(),
                revision: "task:1".to_string(),
                definition: json!({
                    "kind": "mdbase.type",
                    "name": "task",
                    "version": 1,
                    "match": {"path_glob": "tasks/*.md"},
                    "schema": {"dialect": "json-schema-2020-12", "value": {"type": "object"}}
                }),
                schema: json!({"type": "object"}),
            }],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn canonical_view_compiles_to_the_closed_query_plan() {
        let view = CanonicalRecordInput {
            stable_id: Some("view-id".to_string()),
            path: "views/tasks.md".to_string(),
            document: "---\ntype: view\nid: tasks\nversion: 1\nname: Tasks\nquery:\n  types: [task]\nviews:\n  - id: open\n    name: Open\n    where: record.status == 'open'\n    order_by:\n      - field: file.path\n---\n"
                .to_string(),
            file_size: 0,
            file_mtime: None,
        };
        let outcome = catalog()
            .plan_hosted_canonical_view(
                &json!({"path": "views/tasks.md", "view": "open", "limit": 25}),
                &view,
                None,
            )
            .unwrap();
        let HostedCanonicalViewPlanning::Planned { plan } = outcome else {
            panic!("view should compile")
        };
        assert_eq!(plan.view_id, "open");
        assert_eq!(plan.query.page_size, 25);
        assert!(plan.invocation_digest.starts_with("sha256:"));
        let task = CanonicalRecordInput {
            stable_id: Some("task-id".to_string()),
            path: "tasks/open.md".to_string(),
            document: "---\nstatus: open\n---\nTask body\n".to_string(),
            file_size: 0,
            file_mtime: None,
        };
        let evaluated = catalog()
            .evaluate_hosted_residual_with_context(&plan.query, &task, Some(&view))
            .unwrap();
        assert!(evaluated.matched);
    }
}
