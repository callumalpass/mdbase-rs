//! Backfill operation (§12.8).

use std::collections::{HashMap, HashSet};

use crate::errors::*;

use crate::frontmatter::serializer;
use crate::Collection;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeKind {
    Default,
    Generated,
}

fn invalid_record_detail(
    path: &str,
    invalid: crate::record_load::InvalidRecordView<'_>,
) -> serde_json::Value {
    let reason = match invalid {
        crate::record_load::InvalidRecordView::Frontmatter { reason, .. } => reason,
        crate::record_load::InvalidRecordView::InvalidUtf8 { .. } => {
            crate::record_load::InvalidRecordReason::InvalidUtf8
        }
    };
    let (code, message) = match reason {
        crate::record_load::InvalidRecordReason::InvalidUtf8 => (
            "file_read_failed",
            "Collection record could not be read".to_string(),
        ),
        _ => (
            INVALID_FRONTMATTER,
            format!("Invalid frontmatter: {}", reason.as_str()),
        ),
    };
    serde_json::json!({
        "path": path,
        "status": "failed",
        "error": {"code": code, "message": message},
    })
}

#[cfg(test)]
type PlanningHook = Box<dyn FnOnce() + Send>;

#[cfg(test)]
fn planning_hooks(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, PlanningHook>> {
    static HOOKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, PlanningHook>>,
    > = std::sync::OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

#[cfg(test)]
fn inject_planning_hook(root: &std::path::Path, hook: impl FnOnce() + Send + 'static) {
    planning_hooks()
        .lock()
        .expect("backfill planning hook lock")
        .insert(root.to_path_buf(), Box::new(hook));
}

#[cfg(test)]
fn run_planning_hook(root: &std::path::Path) {
    let hook = planning_hooks()
        .lock()
        .expect("backfill planning hook lock")
        .remove(root);
    if let Some(hook) = hook {
        hook();
    }
}

struct BackfillPlan {
    path: String,
    expected_revision: String,
    output: String,
    effective: serde_json::Value,
    type_names: Vec<String>,
    changed_fields: Vec<String>,
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
fn injected_backfill_replacements(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>> {
    static REPLACEMENTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    REPLACEMENTS.get_or_init(Default::default)
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
pub(crate) fn inject_backfill_replacement(path: &std::path::Path, replacement: std::path::PathBuf) {
    injected_backfill_replacements()
        .lock()
        .expect("backfill replacement lock")
        .insert(path.to_path_buf(), replacement);
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
fn apply_injected_backfill_replacement(path: &std::path::Path) {
    if let Some(replacement) = injected_backfill_replacements()
        .lock()
        .expect("backfill replacement lock")
        .remove(path)
    {
        std::fs::rename(replacement, path).expect("injected backfill replacement");
    }
}

impl Collection {
    pub(crate) fn backfill_legacy(&self, input: &serde_json::Value) -> serde_json::Value {
        self.backfill_contextual(input, None, &mut Vec::new())
    }

    fn backfill_contextual(
        &self,
        input: &serde_json::Value,
        context: Option<&crate::runtime::OperationContext>,
        typed_diagnostics: &mut Vec<crate::api::Diagnostic>,
    ) -> serde_json::Value {
        if let Some(error) = context.and_then(|context| context.check().err()) {
            return op_error(error.code(), &error.to_string());
        }
        let type_filter = input.get("type").and_then(|v| v.as_str());
        let where_clause = input.get("where");
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let apply_defaults = input
            .get("apply")
            .and_then(|v| v.get("defaults"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let apply_generated = input
            .get("apply")
            .and_then(|v| v.get("generated"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let fields_filter: Option<HashSet<String>> =
            input.get("fields").and_then(|v| v.as_array()).map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            });

        if type_filter.is_none() && where_clause.is_none() {
            return op_error(INVALID_REQUEST, "backfill requires 'type' or 'where'");
        }

        let filter_types: Vec<String> = type_filter
            .map(|t| vec![t.to_lowercase()])
            .unwrap_or_default();

        let snapshot = match self.capture_collection_snapshot_current() {
            Ok(snapshot) => snapshot,
            Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
        };
        let matching_paths =
            match self.query_matching_paths_with_types(&snapshot, where_clause, &filter_types) {
                Ok(paths) => paths,
                Err(error) => return op_error("collection_snapshot_failed", &error.to_string()),
            };
        let total = matching_paths.len();
        if total == 0 {
            return serde_json::json!({
                "batch_result": {
                    "total": 0,
                    "succeeded": 0,
                    "failed": 0,
                    "skipped": 0,
                    "details": [],
                }
            });
        }

        let mut plans: Vec<BackfillPlan> = Vec::new();
        let mut skipped = 0usize;
        let mut noop_success = 0usize;
        let mut planning_failed = 0usize;
        let mut details: Vec<serde_json::Value> = Vec::new();
        let mut generated = crate::generated::GeneratedValueContext::from_snapshot(self, &snapshot);

        for path in &matching_paths {
            #[cfg(test)]
            run_planning_hook(self.root());
            if let Some(error) = context.and_then(|context| context.check().err()) {
                return op_error(error.code(), &error.to_string());
            }
            let Some(entry) = snapshot.entry(path) else {
                return op_error(
                    "collection_snapshot_failed",
                    "selected backfill record is absent from its snapshot",
                );
            };
            let Some(raw_frontmatter) = entry.raw_frontmatter().cloned() else {
                planning_failed += 1;
                if let Some(invalid) = entry.invalid() {
                    details.push(invalid_record_detail(entry.relative_path(), invalid));
                }
                continue;
            };
            let raw_obj = raw_frontmatter.as_object().cloned().unwrap_or_default();
            let type_names = entry.type_names().to_vec();
            let body = entry.body().unwrap_or_default();
            let had_bom = entry.had_bom().unwrap_or(false);

            let mut working = raw_obj.clone();
            let mut candidate_generated = generated.clone();
            let mut changes: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
            let mut change_kinds: HashMap<String, ChangeKind> = HashMap::new();

            if apply_generated {
                let mut missing_generated = HashSet::new();
                for type_name in &type_names {
                    if let Some(type_def) = self.types.get(type_name) {
                        for (field_name, field_def) in &type_def.fields {
                            if field_def.generated.is_some()
                                && !raw_obj.contains_key(field_name)
                                && fields_filter
                                    .as_ref()
                                    .is_none_or(|filter| filter.contains(field_name))
                            {
                                missing_generated.insert(field_name.clone());
                            }
                        }
                    }
                }
                let generated_fields = match candidate_generated.apply_generated_filtered(
                    self,
                    &mut working,
                    &type_names,
                    true,
                    Some(path),
                    Some(&missing_generated),
                ) {
                    Ok(fields) => fields,
                    Err(error) => return op_error(error.code(), &error.to_string()),
                };
                for field_name in generated_fields {
                    let value = working
                        .get(&field_name)
                        .cloned()
                        .expect("generated field was inserted");
                    changes.insert(field_name.clone(), value);
                    change_kinds.insert(field_name, ChangeKind::Generated);
                }
            }

            if apply_defaults {
                for type_name in &type_names {
                    if let Some(type_def) = self.types.get(type_name) {
                        let defaults =
                            type_def
                                .read_defaults
                                .iter()
                                .map(|(field_name, value)| (field_name.clone(), value.clone()))
                                .chain(type_def.fields.iter().filter_map(
                                    |(field_name, field_def)| {
                                        field_def
                                            .default
                                            .clone()
                                            .map(|value| (field_name.clone(), value))
                                    },
                                ));
                        for (field_name, value) in defaults {
                            if fields_filter
                                .as_ref()
                                .is_some_and(|filter| !filter.contains(&field_name))
                                || working.contains_key(&field_name)
                            {
                                continue;
                            }
                            working.insert(field_name.clone(), value.clone());
                            changes.insert(field_name.clone(), value);
                            change_kinds.insert(field_name, ChangeKind::Default);
                        }
                    }
                }
            }

            if changes.is_empty() {
                if fields_filter.is_some() {
                    skipped += 1;
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "skipped",
                        "reason": "No missing fields to backfill",
                    }));
                } else {
                    noop_success += 1;
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "success",
                        "reason": "No missing fields to backfill",
                    }));
                }
                continue;
            }

            // Build write map honoring write_defaults/write_nulls
            let mut write_obj = raw_obj.clone();
            for (field, value) in &changes {
                if change_kinds.get(field) == Some(&ChangeKind::Default)
                    && self.spec_profile() == crate::SpecProfile::V02
                    && !self.settings.write_defaults
                {
                    continue;
                }
                if self.settings.write_nulls == "omit" && value.is_null() {
                    continue;
                }
                write_obj.insert(field.clone(), value.clone());
            }

            let effective = self.coerce_types(
                &self.apply_defaults(&serde_json::Value::Object(write_obj.clone()), &type_names),
                &type_names,
            );
            let authored_mapping = entry
                .outcome()
                .document()
                .and_then(|document| {
                    crate::frontmatter::parser::parse_document(document)
                        .frontmatter
                        .and_then(|value| value.as_mapping().cloned())
                })
                .unwrap_or_default();
            let yaml_mapping = serializer::reconcile_json_object(&authored_mapping, &write_obj);
            let output = match serializer::serialize_document_with_bom(had_bom, &yaml_mapping, body)
            {
                Ok(output) => output,
                Err(error) => {
                    planning_failed += 1;
                    details.push(serde_json::json!({
                        "path": path,
                        "status": "failed",
                        "error": {
                            "code": FRONTMATTER_SERIALIZATION_FAILED,
                            "message": error.to_string()
                        }
                    }));
                    continue;
                }
            };
            generated = candidate_generated;
            plans.push(BackfillPlan {
                path: path.to_string(),
                expected_revision: entry.facts().revision.clone(),
                output,
                effective,
                type_names,
                changed_fields: changes.keys().cloned().collect(),
            });
        }

        // Validate the complete final effective corpus before the first write.
        if self.settings.default_validation == "error" {
            let mut corpus = snapshot
                .entries()
                .iter()
                .filter_map(|entry| {
                    entry
                        .effective_frontmatter()
                        .map(|frontmatter| (entry.relative_path().to_string(), frontmatter.clone()))
                })
                .collect::<HashMap<_, _>>();
            for plan in &plans {
                corpus.insert(plan.path.clone(), plan.effective.clone());
            }
            let corpus = corpus.into_iter().collect::<Vec<_>>();
            let mut resolved_files = snapshot.resolved_files_data();
            for plan in &plans {
                if let Some(file) = resolved_files
                    .iter_mut()
                    .find(|file| file.path == plan.path)
                {
                    file.frontmatter = plan.effective.clone();
                }
            }
            let resolution_index = self.build_link_resolution_index(&resolved_files);
            let mut issues = Vec::new();
            for plan in &plans {
                let validation = self.validate(&plan.effective, &plan.type_names, &plan.path);
                issues.extend(validation.issues);
                issues.extend(self.check_uniqueness_in_corpus(
                    &plan.effective,
                    &plan.type_names,
                    &plan.path,
                    &corpus,
                ));
                issues.extend(self.check_link_exists(
                    &plan.effective,
                    &plan.type_names,
                    &plan.path,
                    &resolution_index,
                ));
            }
            if issues
                .iter()
                .any(|issue| issue.severity == crate::errors::Severity::Error)
            {
                return validation_failed_error(&issues);
            }
        }

        if dry_run {
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": plans.len() + noop_success,
                    "failed": planning_failed,
                    "skipped": skipped,
                    "details": details,
                }
            });
        }

        let mut succeeded = noop_success;
        let mut failed = planning_failed;

        if let Some(context) = context {
            if !plans.is_empty() {
                let operations = plans
                    .iter()
                    .map(|plan| {
                        let mut request = crate::api::UpdateRequest::replace_document(
                            crate::api::CollectionPath::new(&plan.path)
                                .expect("snapshot paths are canonical collection paths"),
                            plan.output.clone(),
                        );
                        request.if_revision = Some(
                            crate::api::Revision::parse(plan.expected_revision.clone())
                                .expect("snapshot revisions are opaque non-empty tokens"),
                        );
                        crate::api::BatchOperation::Update(request)
                    })
                    .collect::<Vec<_>>();
                let request = crate::api::BatchRequest {
                    operations,
                    allow_partial: false,
                    dry_run: false,
                };
                match crate::mutation::batch_with_context(self, request, context) {
                    Ok(outcome) => typed_diagnostics.extend(outcome.diagnostics),
                    Err(error) => {
                        if let Some(diagnostic) = error.diagnostics().first() {
                            return op_error(diagnostic.code.as_str(), &diagnostic.message);
                        }
                        return op_error("backfill_failed", &error.to_string());
                    }
                }
            }
            succeeded += plans.len();
            details.extend(plans.into_iter().map(|plan| {
                serde_json::json!({
                    "path": plan.path,
                    "status": "success",
                    "changed_fields": plan.changed_fields,
                })
            }));
            return serde_json::json!({
                "batch_result": {
                    "total": total,
                    "succeeded": succeeded,
                    "failed": failed,
                    "skipped": skipped,
                    "details": details,
                }
            });
        }

        for plan in plans {
            #[cfg(all(test, feature = "legacy-collection-mutation"))]
            apply_injected_backfill_replacement(&self.root.join(&plan.path));
            let current = match crate::record_load::load_record(self, &plan.path) {
                Ok(current) => current,
                Err(error) => {
                    failed += 1;
                    details.push(serde_json::json!({
                        "path": plan.path,
                        "status": "failed",
                        "error": {
                            "code": "file_read_failed",
                            "message": format!("Failed to revalidate record: {error}")
                        }
                    }));
                    continue;
                }
            };
            if current.facts().revision != plan.expected_revision {
                failed += 1;
                details.push(serde_json::json!({
                    "path": plan.path,
                    "status": "failed",
                    "error": {
                        "code": CONCURRENT_MODIFICATION,
                        "message": "File was modified externally"
                    }
                }));
                continue;
            }
            if let Err(e) = self
                .held_root()
                .atomic_write(std::path::Path::new(&plan.path), plan.output.as_bytes())
            {
                failed += 1;
                details.push(serde_json::json!({
                    "path": plan.path,
                    "status": "failed",
                    "error": { "code": "io_error", "message": e.to_string() }
                }));
            } else {
                succeeded += 1;
                details.push(serde_json::json!({
                    "path": plan.path,
                    "status": "success",
                    "changed_fields": plan.changed_fields,
                }));
            }
        }

        serde_json::json!({
            "batch_result": {
                "total": total,
                "succeeded": succeeded,
                "failed": failed,
                "skipped": skipped,
                "details": details,
            }
        })
    }
}

pub(crate) fn execute(
    collection: &Collection,
    request: crate::api::BackfillRequest,
    context: &crate::runtime::OperationContext,
) -> crate::api::MdbaseResult<crate::api::OperationOutcome<crate::api::BackfillResult>> {
    if request.type_name.is_none() && request.where_expression.is_none() {
        return Err(crate::api::MdbaseError::Operation {
            diagnostics: vec![crate::api::Diagnostic {
                severity: crate::api::Severity::Error,
                code: crate::api::DiagnosticCode::new(INVALID_REQUEST),
                message: "backfill requires 'type' or 'where'".to_string(),
                path: None,
                field: None,
                type_name: None,
                schema_location: None,
                details: None,
            }],
        });
    }
    let mut input = serde_json::Map::new();
    if let Some(value) = request.type_name {
        input.insert("type".to_string(), serde_json::Value::String(value));
    }
    if let Some(value) = request.where_expression {
        input.insert("where".to_string(), serde_json::Value::String(value));
    }
    if let Some(fields) = request.fields {
        input.insert(
            "fields".to_string(),
            serde_json::Value::Array(fields.into_iter().map(serde_json::Value::String).collect()),
        );
    }
    if request.dry_run {
        input.insert("dry_run".to_string(), serde_json::Value::Bool(true));
    }
    if request.apply_defaults.is_some() || request.apply_generated.is_some() {
        let mut apply = serde_json::Map::new();
        if let Some(value) = request.apply_defaults {
            apply.insert("defaults".to_string(), serde_json::Value::Bool(value));
        }
        if let Some(value) = request.apply_generated {
            apply.insert("generated".to_string(), serde_json::Value::Bool(value));
        }
        input.insert("apply".to_string(), serde_json::Value::Object(apply));
    }

    let input = serde_json::Value::Object(input);
    let mut diagnostics = Vec::new();
    let value =
        context.scope(|| collection.backfill_contextual(&input, Some(context), &mut diagnostics));
    if let Some(error) = value.get("error") {
        let code = error
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("backfill_failed");
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Backfill failed.");
        return Err(crate::api::MdbaseError::Operation {
            diagnostics: vec![crate::api::Diagnostic {
                severity: crate::api::Severity::Error,
                code: crate::api::DiagnosticCode::new(code),
                message: message.to_string(),
                path: None,
                field: None,
                type_name: None,
                schema_location: None,
                details: Some(error.clone()),
            }],
        });
    }
    let result =
        serde_json::from_value(value).map_err(|error| crate::api::MdbaseError::InvalidResult {
            message: format!("could not decode typed backfill result: {error}"),
        })?;
    Ok(crate::api::OperationOutcome {
        value: result,
        diagnostics,
    })
}

#[cfg(test)]
mod typed_tests {
    use super::inject_planning_hook;
    use crate::api::BackfillRequest;
    use crate::runtime::{OperationContext, OperationDeadline};
    use crate::{Collection, OperationCancellation};
    use std::fs;
    use std::time::Duration;

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/task.md"),
            r#"---
kind: mdbase.type
name: task
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [type, title]
    properties:
      type: { const: task }
      title: { type: string }
      status: { type: string }
collection:
  read_defaults:
    status: open
---
"#,
        )
        .unwrap();
        for name in ["a", "b"] {
            fs::write(
                root.path().join(format!("{name}.md")),
                format!("---\ntype: task\ntitle: {name}\n---\n{name}\n"),
            )
            .unwrap();
        }
        root
    }

    fn request() -> BackfillRequest {
        BackfillRequest {
            type_name: Some("task".to_string()),
            ..BackfillRequest::default()
        }
    }

    fn context(token: &OperationCancellation) -> OperationContext {
        OperationContext::new(token, OperationDeadline::after(Duration::from_secs(30)))
    }

    fn assert_not_backfilled(root: &std::path::Path, name: &str) {
        assert!(!fs::read_to_string(root.join(format!("{name}.md")))
            .unwrap()
            .contains("status:"));
    }

    #[test]
    fn cancellation_during_planning_writes_nothing() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        let token = OperationCancellation::new();
        let cancel = token.clone();
        inject_planning_hook(root.path(), move || cancel.cancel());

        let error = collection
            .typed()
            .unwrap()
            .backfill_with_context(request(), &context(&token))
            .unwrap_err();
        assert_eq!(error.diagnostics()[0].code.as_str(), "operation_cancelled");
        assert_not_backfilled(root.path(), "a");
        assert_not_backfilled(root.path(), "b");
    }

    #[test]
    fn cancellation_immediately_precommit_writes_nothing() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        let token = OperationCancellation::new();
        let cancel = token.clone();
        crate::mutation::inject_precommit_hook(root.path(), move || cancel.cancel());

        let error = collection
            .typed()
            .unwrap()
            .backfill_with_context(request(), &context(&token))
            .unwrap_err();
        assert_eq!(error.diagnostics()[0].code.as_str(), "operation_cancelled");
        assert_not_backfilled(root.path(), "a");
        assert_not_backfilled(root.path(), "b");
    }

    #[test]
    fn conflict_rolls_back_every_planned_record() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        let conflicted = root.path().join("a.md");
        crate::mutation::inject_precommit_hook(root.path(), move || {
            fs::write(&conflicted, "external\n").unwrap();
        });

        let error = collection.typed().unwrap().backfill(request()).unwrap_err();
        assert_eq!(
            error.diagnostics()[0].code.as_str(),
            "concurrent_modification"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("a.md")).unwrap(),
            "external\n"
        );
        assert_not_backfilled(root.path(), "b");
    }

    #[test]
    fn interrupted_commit_recovers_the_complete_backfill() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        crate::transactions::inject_commit_crash_after(root.path(), 1);
        let error = collection.typed().unwrap().backfill(request()).unwrap_err();
        assert_eq!(error.diagnostics()[0].code.as_str(), "simulated_crash");
        drop(collection);

        let recovered = Collection::open(root.path()).unwrap();
        drop(recovered);
        for name in ["a", "b"] {
            assert!(fs::read_to_string(root.path().join(format!("{name}.md")))
                .unwrap()
                .contains("status:"));
        }
    }

    #[test]
    fn cleanup_deferred_is_returned_as_a_typed_warning() {
        let root = fixture();
        let collection = Collection::open(root.path()).unwrap();
        crate::transactions::inject_cleanup_deferred(root.path());

        let outcome = collection.typed().unwrap().backfill(request()).unwrap();
        assert!(outcome.diagnostics.iter().any(|diagnostic| {
            diagnostic.code.as_str() == "transaction_cleanup_deferred"
                && diagnostic.severity == crate::api::Severity::Warning
        }));
    }
}

#[cfg(all(test, feature = "legacy-collection-mutation"))]
mod tests {
    use super::inject_backfill_replacement;
    use crate::Collection;
    use serde_json::json;
    use std::fs;

    #[test]
    fn external_edit_after_planning_is_never_overwritten_and_dry_run_does_not_reload() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("mdbase.yaml"),
            "spec_version: 0.2.0\nsettings:\n  validation: error\n  write_defaults: true\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("_types")).unwrap();
        fs::write(
            root.path().join("_types/item.md"),
            "---\nname: item\nfields:\n  value: { type: string, default: filled }\n---\n",
        )
        .unwrap();
        let record = root.path().join("record.md");
        fs::write(&record, "---\ntype: item\n---\noriginal\n").unwrap();
        let replacement = root.path().join("replacement.tmp");
        fs::write(&replacement, "---\ntype: item\n---\nexternal\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        inject_backfill_replacement(&record, replacement);

        let dry_run = collection.backfill(&json!({"type": "item", "dry_run": true}));
        assert_eq!(dry_run["batch_result"]["succeeded"], 1, "{dry_run:#}");
        assert!(fs::read_to_string(&record).unwrap().contains("original"));

        let result = collection.backfill(&json!({"type": "item"}));
        assert_eq!(result["batch_result"]["failed"], 1, "{result:#}");
        assert_eq!(
            result["batch_result"]["details"][0]["error"]["code"], "concurrent_modification",
            "{result:#}"
        );
        let persisted = fs::read_to_string(record).unwrap();
        assert!(persisted.contains("external"), "{persisted}");
        assert!(!persisted.contains("value:"), "{persisted}");
    }
}
