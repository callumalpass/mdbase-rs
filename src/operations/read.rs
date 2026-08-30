//! Read operation (§12.2).

use crate::api::operations::ReadInput;
use crate::api::{
    CollectionPath, Diagnostic, DiagnosticCode, MdbaseError as TypedError, OperationOutcome,
    ReadRequest, RecordDocument, RecordFile, Revision, Severity,
};
use crate::errors::*;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json, FrontmatterState};
use crate::operations::{ensure_safe_relative_path, readable_record_path};
use crate::record_load::{InvalidRecordView, RecordLoadView};
use crate::Collection;
use std::path::Path;

/// Provider-supplied file facts for one canonical record read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordFileFacts {
    pub size: u64,
    pub mtime: Option<String>,
}

/// Authoritative storage input for one canonical typed point read.
pub(crate) enum TypedReadSource<'a> {
    Filesystem,
    Exact {
        canonical_path: &'a str,
        document: &'a str,
        file_facts: &'a RecordFileFacts,
    },
    Missing,
}

/// Internal read evaluation retained by the wire adapter so validation-error
/// envelopes can preserve their historical result payload.
pub(crate) struct TypedReadEvaluation {
    pub value: Option<RecordDocument>,
    pub diagnostics: Vec<Diagnostic>,
    pub valid: bool,
}

impl TypedReadEvaluation {
    pub(crate) fn into_outcome(self) -> Result<OperationOutcome<RecordDocument>, TypedError> {
        if !self.valid {
            return Err(TypedError::Operation {
                diagnostics: self.diagnostics,
            });
        }
        let value = self.value.ok_or_else(|| TypedError::InvalidResult {
            message: "successful read did not produce a record document".to_string(),
        })?;
        Ok(OperationOutcome {
            value,
            diagnostics: self.diagnostics,
        })
    }
}

/// Evaluate one canonical point read from either the collection capability or
/// one provider-owned exact record. This is the sole v0.3 read evaluator.
pub(crate) fn evaluate_typed_read(
    collection: &Collection,
    request: &ReadRequest,
    source: TypedReadSource<'_>,
) -> TypedReadEvaluation {
    let requested = request.path.as_str();
    if collection.validate_record_path(requested).is_err() {
        return read_error(
            FILE_NOT_FOUND,
            format!("File not found: {requested}"),
            Some(requested),
        );
    }
    let (path, document, facts) = match source {
        TypedReadSource::Filesystem => {
            let loaded = match crate::record_load::load_record_no_follow(collection, requested) {
                Ok(Some(loaded)) => loaded,
                Ok(None) => {
                    return read_error(
                        FILE_NOT_FOUND,
                        format!("File not found: {requested}"),
                        Some(requested),
                    )
                }
                Err(_) => {
                    return read_error(
                        INVALID_FRONTMATTER,
                        "File contains invalid UTF-8",
                        Some(requested),
                    )
                }
            };
            if matches!(
                loaded.view(),
                RecordLoadView::Invalid(InvalidRecordView::InvalidUtf8 { .. })
            ) {
                return read_error(
                    INVALID_FRONTMATTER,
                    "File contains invalid UTF-8",
                    Some(requested),
                );
            }
            let document = loaded
                .document()
                .expect("UTF-8 record loads retain their exact document")
                .to_string();
            let loaded_facts = loaded.facts();
            let facts = RecordFileFacts {
                size: loaded_facts.size,
                mtime: format_mtime(loaded_facts.mtime_ns),
            };
            (requested.to_string(), document, facts)
        }
        TypedReadSource::Exact {
            canonical_path,
            document,
            file_facts,
        } => {
            if canonical_path != requested {
                return read_error(
                    "record_identity_mismatch",
                    "The supplied record does not match the requested canonical path.",
                    Some(requested),
                );
            }
            (
                canonical_path.to_string(),
                document.to_string(),
                file_facts.clone(),
            )
        }
        TypedReadSource::Missing => {
            return read_error(
                FILE_NOT_FOUND,
                format!("File not found: {requested}"),
                Some(requested),
            )
        }
    };

    evaluate_document(
        collection,
        &request.path,
        &path,
        document,
        facts,
        request.include_document,
    )
}

fn evaluate_document(
    collection: &Collection,
    canonical_path: &CollectionPath,
    path: &str,
    document: String,
    file_facts: RecordFileFacts,
    include_document: bool,
) -> TypedReadEvaluation {
    let parsed = parse_document(&document);
    let mut diagnostics = Vec::new();
    let persisted_frontmatter = match parsed.frontmatter_state() {
        FrontmatterState::InvalidYaml => {
            return read_error(
                INVALID_FRONTMATTER,
                "Failed to parse YAML frontmatter",
                Some(path),
            )
        }
        FrontmatterState::Mapping(mapping) => yaml_mapping_to_json(mapping),
        FrontmatterState::Null => {
            if collection.settings.default_validation == "error" {
                return read_error(INVALID_FRONTMATTER, "Frontmatter is null", Some(path));
            }
            serde_json::json!({})
        }
        FrontmatterState::Absent => serde_json::json!({}),
        FrontmatterState::NonMapping(_) => match collection.settings.default_validation.as_str() {
            "off" => serde_json::json!({}),
            "warn" => {
                diagnostics.push(read_diagnostic(
                    Severity::Warning,
                    INVALID_FRONTMATTER,
                    "Frontmatter must be a YAML mapping",
                    Some(path),
                ));
                serde_json::json!({})
            }
            _ => {
                return read_error(
                    INVALID_FRONTMATTER,
                    "Frontmatter must be a YAML mapping",
                    Some(path),
                )
            }
        },
    };

    let (type_names, match_failures) =
        collection.determine_types_for_path_checked(&persisted_frontmatter, Some(path));
    let effective = collection.coerce_types(
        &collection.apply_defaults(&persisted_frontmatter, &type_names),
        &type_names,
    );
    let effective = collection.evaluate_computed_fields(
        effective,
        &type_names,
        path,
        Some(parsed.body.as_str()),
    );
    let validation = if collection.settings.default_validation == "off" {
        ValidationResult {
            valid: true,
            issues: Vec::new(),
        }
    } else {
        collection.validate(&persisted_frontmatter, &type_names, path)
    };
    let validation_severity = if collection.settings.default_validation == "warn" {
        Severity::Warning
    } else {
        Severity::Error
    };
    let mut validation_diagnostics = validation
        .issues
        .iter()
        .map(|issue| Diagnostic {
            severity: validation_severity,
            code: DiagnosticCode::new(issue.code.clone()),
            message: issue.message.clone(),
            path: issue.path.clone().or_else(|| Some(path.to_string())),
            field: issue.field.clone(),
            type_name: issue.type_name.clone(),
            schema_location: None,
            details: None,
        })
        .collect::<Vec<_>>();
    validation_diagnostics.append(&mut diagnostics);
    diagnostics =
        validation_diagnostics
            .into_iter()
            .fold(Vec::new(), |mut deduplicated, diagnostic| {
                if !deduplicated.contains(&diagnostic) {
                    deduplicated.push(diagnostic);
                }
                deduplicated
            });
    diagnostics.extend(
        match_failures
            .into_iter()
            .map(|(type_name, failure)| Diagnostic {
                severity: Severity::Warning,
                code: DiagnosticCode::new("expression_evaluation_error"),
                message: format!(
                    "Type '{type_name}' match expression failed: {}",
                    failure.message
                ),
                path: Some(path.to_string()),
                field: Some("match.expr".to_string()),
                type_name: Some(type_name),
                schema_location: None,
                details: Some(serde_json::json!({
                    "context": "match",
                    "evaluator_code": failure.code,
                })),
            }),
    );

    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let folder = Path::new(path)
        .parent()
        .and_then(|parent| parent.to_str())
        .unwrap_or("")
        .to_string();
    let value = RecordDocument {
        path: canonical_path.clone(),
        revision: Revision::parse(crate::v03::revision(document.as_bytes()))
            .expect("sha256 revisions are non-empty"),
        types: type_names,
        frontmatter: persisted_frontmatter,
        effective_frontmatter: effective,
        body: parsed.body,
        document: include_document.then_some(document),
        file: RecordFile {
            name: file_name,
            folder,
            size: file_facts.size,
            mtime: file_facts.mtime.unwrap_or_default(),
        },
    };
    TypedReadEvaluation {
        value: Some(value),
        diagnostics,
        valid: collection.settings.default_validation == "warn" || validation.valid,
    }
}

fn read_error(code: &str, message: impl Into<String>, path: Option<&str>) -> TypedReadEvaluation {
    TypedReadEvaluation {
        value: None,
        diagnostics: vec![read_diagnostic(Severity::Error, code, message, path)],
        valid: false,
    }
}

fn read_diagnostic(
    severity: Severity,
    code: &str,
    message: impl Into<String>,
    path: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        severity,
        code: DiagnosticCode::new(code),
        message: message.into(),
        path: path.map(str::to_string),
        field: None,
        type_name: None,
        schema_location: None,
        details: None,
    }
}

fn format_mtime(mtime_ns: i64) -> Option<String> {
    let seconds = mtime_ns.div_euclid(1_000_000_000);
    let nanos = mtime_ns.rem_euclid(1_000_000_000) as u32;
    chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, nanos)
        .map(|datetime| datetime.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

impl Collection {
    /// Read a file (§12.2).
    ///
    /// Legacy JSON compatibility only. Canonical typed and v0.3 wire reads use
    /// the version-neutral evaluator directly.
    pub fn read(&self, input: &serde_json::Value) -> serde_json::Value {
        let input = match ReadInput::parse(input) {
            Ok(parsed) => parsed,
            Err(err) => return err,
        };
        if let Err(error) = ensure_safe_relative_path(&input.path, self.spec_profile) {
            return error;
        }
        if let Err(error) =
            crate::operations::ensure_no_symlink_components_held_diagnostic(self, &input.path)
        {
            return op_error(&error.code, &error.message);
        }
        let path = match readable_record_path(self, &input.path) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let content = match self.held_root().read_string(path.to_path_buf()) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return op_error(INVALID_FRONTMATTER, "File contains invalid UTF-8")
            }
            Err(_) => return op_error(FILE_NOT_FOUND, "Failed to read: entity not found"),
        };

        let file_metadata = self
            .held_root()
            .open_file(&path.to_path_buf())
            .and_then(|file| file.metadata())
            .ok();
        let file_facts = RecordFileFacts {
            size: file_metadata
                .as_ref()
                .map(|metadata| metadata.len())
                .unwrap_or(0),
            mtime: file_metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok())
                .map(|time| {
                    let datetime: chrono::DateTime<chrono::Utc> = time.into();
                    datetime.format("%Y-%m-%dT%H:%M:%SZ").to_string()
                }),
        };
        self.read_document(path.as_str(), &content, &file_facts, input.include_document)
    }

    /// Evaluate one exact Markdown record without discovering any other record.
    ///
    /// Retained behind [`Collection::read`] for v0.2 compatibility and rename
    /// hydration only; canonical reads use `evaluate_typed_read`.
    /// Storage providers validate and fetch the requested identity, then supply
    /// the exact document and non-semantic file facts from the same snapshot.
    pub(crate) fn read_document(
        &self,
        path: &str,
        content: &str,
        file_facts: &RecordFileFacts,
        include_document: bool,
    ) -> serde_json::Value {
        let doc = parse_document(content);

        // Get frontmatter as JSON
        let mut warnings: Vec<serde_json::Value> = Vec::new();
        let persisted_frontmatter = match doc.frontmatter_state() {
            FrontmatterState::InvalidYaml => {
                return op_error(INVALID_FRONTMATTER, "Failed to parse YAML frontmatter")
            }
            FrontmatterState::Mapping(mapping) => yaml_mapping_to_json(mapping),
            FrontmatterState::Null => {
                let validation_level = &self.settings.default_validation;
                if validation_level == "error" {
                    return op_error(INVALID_FRONTMATTER, "Frontmatter is null");
                }
                serde_json::json!({})
            }
            FrontmatterState::Absent => serde_json::json!({}),
            FrontmatterState::NonMapping(_) => {
                // Non-mapping frontmatter (list, scalar) - structural error
                let validation_level = &self.settings.default_validation;
                if validation_level == "off" {
                    // At "off" level, treat as empty frontmatter silently
                    serde_json::json!({})
                } else if validation_level == "warn" {
                    // At "warn" level, treat as empty with warning
                    warnings.push(serde_json::json!({
                        "code": INVALID_FRONTMATTER,
                        "message": "Frontmatter must be a YAML mapping",
                    }));
                    serde_json::json!({})
                } else {
                    // At "error" level, non-mapping frontmatter is an error
                    return op_error(INVALID_FRONTMATTER, "Frontmatter must be a YAML mapping");
                }
            }
        };

        // Determine types (using path for match rule evaluation)
        let type_names = self.determine_types_for_path(&persisted_frontmatter, Some(path));

        // Apply defaults for effective frontmatter
        let effective = self.apply_defaults(&persisted_frontmatter, &type_names);

        // Apply type coercion (§7.16)
        let effective = self.coerce_types(&effective, &type_names);

        // Evaluate computed fields (§5.12)
        let effective =
            self.evaluate_computed_fields(effective, &type_names, path, Some(doc.body.as_str()));

        // File metadata
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let folder = Path::new(path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("");
        // Validation
        let validation_level = &self.settings.default_validation;
        let validation = if validation_level == "off" {
            ValidationResult {
                valid: true,
                issues: Vec::new(),
            }
        } else {
            self.validate(
                if self.spec_profile == crate::SpecProfile::V03 {
                    &persisted_frontmatter
                } else {
                    &effective
                },
                &type_names,
                path,
            )
        };
        let issues_json: Vec<serde_json::Value> =
            validation.issues.iter().map(issue_to_json).collect();

        // At "warn" level, validation issues don't make the result invalid
        let effective_valid = if validation_level == "warn" {
            true
        } else {
            validation.valid
        };

        let mut result = serde_json::json!({
            "path": path,
            "revision": crate::v03::revision(content.as_bytes()),
            "types": type_names,
            "frontmatter": persisted_frontmatter,
            "effective_frontmatter": effective,
            "body": doc.body,
            "file": {
                "name": file_name,
                "folder": folder,
                "size": file_facts.size,
                "mtime": file_facts.mtime.as_deref().unwrap_or(""),
            },
            "valid": effective_valid,
            "validation": {
                "valid": validation.valid,
                "issues": issues_json,
            },
        });

        if include_document {
            result["document"] = serde_json::Value::String(content.to_string());
        }

        if !warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(warnings);
        }

        result
    }
}

#[cfg(test)]
mod typed_tests {
    use super::*;
    use serde_json::{json, Value};

    fn collection(validation: &str) -> (tempfile::TempDir, Collection) {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("_types")).unwrap();
        std::fs::create_dir(root.path().join("tasks")).unwrap();
        std::fs::write(
            root.path().join("mdbase.yaml"),
            format!("spec_version: 0.3.0\nsettings:\n  default_validation: {validation}\n"),
        )
        .unwrap();
        std::fs::write(
            root.path().join("_types/task.md"),
            r#"---
kind: mdbase.type
name: task
version: 1
match:
  path_glob: tasks/*.md
schema:
  dialect: json-schema-2020-12
  value:
    type: object
    required: [title]
    properties:
      title: { type: string }
      status: { type: string, default: open }
collection:
  read_defaults:
    status: open
---
"#,
        )
        .unwrap();
        let opened = Collection::open(root.path()).unwrap();
        (root, opened)
    }

    fn diagnostic_values(diagnostics: &[crate::api::Diagnostic]) -> Value {
        Value::Array(
            diagnostics
                .iter()
                .map(|diagnostic| {
                    let mut value = json!({
                        "severity": match diagnostic.severity {
                            Severity::Error => "error",
                            Severity::Warning => "warning",
                            Severity::Info => "info",
                        },
                        "code": diagnostic.code.as_str(),
                        "message": diagnostic.message,
                    });
                    for (key, optional) in [
                        ("path", diagnostic.path.as_ref()),
                        ("field", diagnostic.field.as_ref()),
                        ("type", diagnostic.type_name.as_ref()),
                        ("schema_location", diagnostic.schema_location.as_ref()),
                    ] {
                        if let Some(value_string) = optional {
                            value[key] = Value::String(value_string.clone());
                        }
                    }
                    if let Some(details) = &diagnostic.details {
                        value["details"] = details.clone();
                    }
                    value
                })
                .collect(),
        )
    }

    fn wire_diagnostic_values(diagnostics: &[crate::diagnostic::Diagnostic]) -> Value {
        serde_json::to_value(diagnostics).unwrap()
    }

    #[test]
    fn typed_and_wire_reads_are_differential_and_load_once() {
        let (root, collection) = collection("off");
        let document = "\u{feff}---\ntitle: One\n---\nBody\n";
        std::fs::write(root.path().join("tasks/one.md"), document).unwrap();

        for include_document in [false, true] {
            let request = if include_document {
                ReadRequest::new("tasks/one.md").unwrap().with_document()
            } else {
                ReadRequest::new("tasks/one.md").unwrap()
            };
            crate::record_load::reset_snapshot_record_loads_for_test();
            let typed = collection.typed().unwrap().read(request.clone()).unwrap();
            assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 1);

            crate::record_load::reset_snapshot_record_loads_for_test();
            let wire = crate::v03::Operations::new(&collection)
                .unwrap()
                .read(&json!({
                    "path": "tasks/one.md",
                    "include_document": include_document,
                }));
            assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 1);
            assert!(wire.valid);
            assert_eq!(serde_json::to_value(&typed.value).unwrap(), wire.result);
            assert_eq!(
                diagnostic_values(&typed.diagnostics),
                wire_diagnostic_values(&wire.diagnostics)
            );
            assert_eq!(typed.value.frontmatter["title"], "One");
            assert_eq!(typed.value.effective_frontmatter["status"], "open");
            assert_eq!(typed.value.body, "Body\n");
            assert_eq!(typed.value.document.is_some(), include_document);
            assert_eq!(
                typed.value.revision.as_str(),
                crate::v03::revision(document.as_bytes())
            );
        }
    }

    #[test]
    fn typed_and_wire_failures_match_for_validation_missing_and_strict_documents() {
        for validation in ["warn", "error"] {
            let (root, collection) = collection(validation);
            std::fs::write(root.path().join("tasks/invalid.md"), "Body\n").unwrap();
            let typed = collection
                .typed()
                .unwrap()
                .read(ReadRequest::new("tasks/invalid.md").unwrap());
            let wire = crate::v03::Operations::new(&collection)
                .unwrap()
                .read(&json!({"path": "tasks/invalid.md"}));
            assert_eq!(typed.is_ok(), validation == "warn");
            assert_eq!(wire.valid, validation == "warn");
            let typed_diagnostics = match typed {
                Ok(outcome) => outcome.diagnostics,
                Err(error) => error.diagnostics().to_vec(),
            };
            assert_eq!(
                diagnostic_values(&typed_diagnostics),
                wire_diagnostic_values(&wire.diagnostics)
            );
        }

        let (root, collection) = collection("error");
        std::fs::write(
            root.path().join("tasks/malformed.md"),
            "---\ntitle: [broken\n---\nBody\n",
        )
        .unwrap();
        std::fs::write(root.path().join("tasks/binary.md"), b"bad\xffutf8").unwrap();
        for path in ["tasks/missing.md", "tasks/malformed.md", "tasks/binary.md"] {
            crate::record_load::reset_snapshot_record_loads_for_test();
            let typed_error = collection
                .typed()
                .unwrap()
                .read(ReadRequest::new(path).unwrap())
                .unwrap_err();
            assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 1);
            crate::record_load::reset_snapshot_record_loads_for_test();
            let wire = crate::v03::Operations::new(&collection)
                .unwrap()
                .read(&json!({"path": path}));
            assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 1);
            assert!(!wire.valid);
            assert_eq!(
                diagnostic_values(typed_error.diagnostics()),
                wire_diagnostic_values(&wire.diagnostics),
                "{path}"
            );
        }
    }

    #[test]
    fn nonmapping_frontmatter_preserves_off_warn_error_policy() {
        for (validation, succeeds, warning) in [
            ("off", true, false),
            ("warn", true, true),
            ("error", false, false),
        ] {
            let (root, collection) = collection(validation);
            std::fs::write(
                root.path().join("tasks/nonmapping.md"),
                "---\n- authored\n---\nBody\n",
            )
            .unwrap();
            let typed = collection
                .typed()
                .unwrap()
                .read(ReadRequest::new("tasks/nonmapping.md").unwrap());
            let wire = crate::v03::Operations::new(&collection)
                .unwrap()
                .read(&json!({"path": "tasks/nonmapping.md"}));
            assert_eq!(typed.is_ok(), succeeds);
            assert_eq!(wire.valid, succeeds);
            assert_eq!(
                wire.diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.severity == "warning"),
                warning
            );
        }
    }

    #[test]
    fn exact_hosted_reads_never_touch_the_filesystem_and_check_identity() {
        let (_root, collection) = collection("off");
        let operations = crate::v03::Operations::new(&collection).unwrap();
        let facts = RecordFileFacts {
            size: 5,
            mtime: Some("2025-01-01T00:00:00Z".to_string()),
        };
        crate::record_load::reset_snapshot_record_loads_for_test();
        let exact = operations.read_record(
            &json!({"path": "tasks/exact.md", "include_document": true}),
            "tasks/exact.md",
            "Body\n",
            &facts,
        );
        assert!(exact.valid, "{exact:#?}");
        assert_eq!(exact.result["document"], "Body\n");
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 0);

        let mismatch = operations.read_record(
            &json!({"path": "tasks/other.md"}),
            "tasks/exact.md",
            "Body\n",
            &facts,
        );
        assert_eq!(mismatch.diagnostics[0].code, "record_identity_mismatch");
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 0);

        let missing = operations.read_record_not_found(&json!({"path": "tasks/missing.md"}));
        assert_eq!(missing.diagnostics[0].code, FILE_NOT_FOUND);
        assert_eq!(crate::record_load::snapshot_record_loads_for_test(), 0);
    }

    #[test]
    fn wire_traversal_stays_path_traversal_while_typed_path_fails_locally() {
        let (_root, collection) = collection("off");
        assert!(matches!(
            ReadRequest::new("../outside.md"),
            Err(TypedError::InvalidPath(_))
        ));
        let wire = crate::v03::Operations::new(&collection)
            .unwrap()
            .read(&json!({"path": "../outside.md"}));
        assert!(!wire.valid);
        assert_eq!(wire.diagnostics[0].code, PATH_TRAVERSAL);
    }
}
