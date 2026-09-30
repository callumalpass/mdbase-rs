//! Cross-record validation: uniqueness (§07 Cross-File Uniqueness) and link
//! existence (§07 Links) on writes.
//!
//! A unique rule compares a record's value with the records in the rule's
//! comparison set: those matching the declaring type (`scope: type`, the
//! default), every record (`scope: collection`), or those under a path glob
//! (`scope: path_glob`). Every uniqueness check in the crate derives its keys
//! here, so the corpus scan, the runtime cache index and hosted projections
//! agree on what conflicts.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde_json::Value;

use crate::errors::{Issue, Severity, DUPLICATE_VALUE};
use crate::types::schema::TypeDef;
use crate::Collection;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum UniqueScope {
    Type,
    Collection,
    PathGlob(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UniqueRule {
    pub field: String,
    pub scope: UniqueScope,
}

/// One value in one comparison set. Two records conflict when a key one must
/// hold alone is a key the other belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct UniqueSetKey {
    /// `type:<name>`, `collection`, or `path_glob:<glob>`.
    pub set: String,
    pub field: String,
    pub value: String,
}

/// A record in a uniqueness comparison: path, effective frontmatter, types.
pub(crate) type UniqueCorpusEntry = (String, Value, Vec<String>);

/// Rules a type declares. `collection.unique` rules keep their scope; a legacy
/// per-field `unique: true` is type-scoped.
pub(crate) fn unique_rules(type_def: &TypeDef) -> Vec<UniqueRule> {
    let mut rules = BTreeSet::new();
    let mut declared = HashSet::new();
    for rule in type_def
        .v03_frontmatter
        .as_ref()
        .and_then(|value| value.pointer("/collection/unique"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(field) = rule.get("field").and_then(Value::as_str) else {
            continue;
        };
        declared.insert(field.to_string());
        let scope = match rule.get("scope").and_then(Value::as_str) {
            Some("collection") => UniqueScope::Collection,
            Some("path_glob") => match rule.get("path_glob").and_then(Value::as_str) {
                Some(glob) => UniqueScope::PathGlob(glob.to_string()),
                None => continue,
            },
            _ => UniqueScope::Type,
        };
        rules.insert(UniqueRule {
            field: field.to_string(),
            scope,
        });
    }
    for (name, field) in &type_def.fields {
        if field.unique && !declared.contains(name) {
            rules.insert(UniqueRule {
                field: name.clone(),
                scope: UniqueScope::Type,
            });
        }
    }
    rules.into_iter().collect()
}

/// The string two unique values are compared by. Null values are exempt.
pub(crate) fn unique_comparable_value(value: &Value) -> Option<String> {
    if value.is_null() {
        return None;
    }
    Some(match value.as_str() {
        Some(value) => value.to_string(),
        None => value.to_string(),
    })
}

fn set_name(scope: &UniqueScope, declaring_type: &str) -> String {
    match scope {
        UniqueScope::Type => format!("type:{declaring_type}"),
        UniqueScope::Collection => "collection".to_string(),
        UniqueScope::PathGlob(glob) => format!("path_glob:{glob}"),
    }
}

fn lowercase(types: &[String]) -> Vec<String> {
    types.iter().map(|name| name.to_lowercase()).collect()
}

impl Collection {
    /// Keys a record must hold alone: one per rule declared by its own types,
    /// with the declaring type.
    pub(crate) fn unique_requirements(
        &self,
        effective: &Value,
        types: &[String],
    ) -> Vec<(UniqueSetKey, String)> {
        let mut requirements = BTreeSet::new();
        for type_name in lowercase(types) {
            let Some(type_def) = self.types.get(&type_name) else {
                continue;
            };
            for rule in unique_rules(type_def) {
                if let Some(value) = crate::field_references::get_value(effective, &rule.field)
                    .and_then(unique_comparable_value)
                {
                    requirements.insert((
                        UniqueSetKey {
                            set: set_name(&rule.scope, &type_name),
                            field: rule.field,
                            value,
                        },
                        type_name.clone(),
                    ));
                }
            }
        }
        requirements.into_iter().collect()
    }

    /// Keys a record contributes to every comparison set it belongs to.
    pub(crate) fn unique_memberships(
        &self,
        effective: &Value,
        types: &[String],
        path: &str,
    ) -> Vec<UniqueSetKey> {
        let types = lowercase(types);
        let path = path.replace('\\', "/");
        let mut keys = BTreeSet::new();
        for (declaring_type, type_def) in &self.types {
            for rule in unique_rules(type_def) {
                let member = match &rule.scope {
                    UniqueScope::Type => types.contains(declaring_type),
                    UniqueScope::Collection => true,
                    UniqueScope::PathGlob(glob) => {
                        crate::matching::glob::portable_glob_match(glob, &path)
                    }
                };
                if !member {
                    continue;
                }
                if let Some(value) = crate::field_references::get_value(effective, &rule.field)
                    .and_then(unique_comparable_value)
                {
                    keys.insert(UniqueSetKey {
                        set: set_name(&rule.scope, declaring_type),
                        field: rule.field,
                        value,
                    });
                }
            }
        }
        keys.into_iter().collect()
    }

    /// Duplicate-value issues for one record against `corpus`, which holds the
    /// other records it is compared with (an entry at `path` is skipped).
    pub(crate) fn unique_value_issues(
        &self,
        effective: &Value,
        types: &[String],
        path: &str,
        corpus: &[UniqueCorpusEntry],
    ) -> Vec<Issue> {
        let requirements = self.unique_requirements(effective, types);
        if requirements.is_empty() {
            return Vec::new();
        }
        let path = path.replace('\\', "/");
        let mut issues = Vec::new();
        for (other_path, other, other_types) in corpus {
            if *other_path == path {
                continue;
            }
            let memberships = self
                .unique_memberships(other, other_types, other_path)
                .into_iter()
                .collect::<HashSet<_>>();
            for (key, declaring_type) in &requirements {
                if memberships.contains(key) {
                    issues.push(duplicate_issue(
                        &path,
                        key,
                        declaring_type,
                        format!(
                            "Duplicate unique value '{}' for field '{}' (also in {})",
                            key.value, key.field, other_path
                        ),
                    ));
                }
            }
        }
        issues
    }

    /// Duplicate-value issues across a whole corpus: every record holding a
    /// key that another record in the same comparison set also holds.
    pub(crate) fn corpus_unique_value_issues(&self, corpus: &[UniqueCorpusEntry]) -> Vec<Issue> {
        let mut members: BTreeMap<UniqueSetKey, BTreeSet<&str>> = BTreeMap::new();
        for (path, effective, types) in corpus {
            for key in self.unique_memberships(effective, types, path) {
                members.entry(key).or_default().insert(path.as_str());
            }
        }
        let mut issues = Vec::new();
        for (path, effective, types) in corpus {
            for (key, declaring_type) in self.unique_requirements(effective, types) {
                let conflicts = members
                    .get(&key)
                    .is_some_and(|paths| paths.iter().any(|other| *other != path));
                if conflicts {
                    issues.push(duplicate_issue(
                        path,
                        &key,
                        &declaring_type,
                        format!(
                            "Duplicate value '{}' for unique field '{}' in type '{}'",
                            key.value, key.field, declaring_type
                        ),
                    ));
                }
            }
        }
        issues
    }
}

impl Collection {
    /// Raw values of the link fields whose rules require their target to exist.
    fn required_link_values(&self, effective: &Value, types: &[String]) -> Vec<String> {
        self.validation_link_checks(effective, &lowercase(types))
            .into_iter()
            .filter(|(_, field, _, _)| field.validate_exists == Some(true))
            .map(|(_, _, _, link)| link)
            .collect()
    }

    /// Uniqueness and link issues for a record the runtime writes, checked
    /// against the cache index of the whole collection rather than the sparse
    /// shadow the write was prepared in.
    pub(crate) fn write_cross_record_issues_indexed(
        &self,
        effective: &Value,
        types: &[String],
        path: &str,
    ) -> Result<Vec<Issue>, crate::cache::CacheError> {
        let mut issues = self.check_uniqueness_indexed(effective, types, path)?;
        let links = self.required_link_values(effective, types);
        if !links.is_empty() {
            let index = crate::cache::runtime::link_candidate_index(self, path, &links)?;
            issues.extend(self.check_link_exists(effective, &lowercase(types), path, &index));
        }
        Ok(issues)
    }

    /// Fails every batch item whose required links do not resolve in the
    /// batch's final state. Returns whether any item failed.
    pub(crate) fn reject_unresolved_links(
        &self,
        result: &mut crate::api::BatchResult,
    ) -> Result<bool, crate::snapshot::SnapshotError> {
        if self.settings.default_validation != "error" {
            return Ok(false);
        }
        let written = result
            .operations
            .iter()
            .filter_map(|item| match &item.result {
                crate::api::BatchOperationResult::Record(record) => Some(record),
                _ => None,
            })
            .collect::<Vec<_>>();
        let issues = self.written_link_issues(&written)?;
        for item in &mut result.operations {
            let crate::api::BatchOperationResult::Record(record) = &item.result else {
                continue;
            };
            let path = record.path.as_str().to_string();
            let failures = issues
                .iter()
                .filter(|issue| issue.path.as_deref() == Some(path.as_str()))
                .map(|issue| crate::mutation::diagnostic_from_issue(issue).into())
                .collect::<Vec<_>>();
            if !failures.is_empty() {
                item.valid = false;
                item.diagnostics.extend(failures);
            }
        }
        result.failed = result.operations.iter().filter(|item| !item.valid).count();
        result.succeeded = result.operations.len() - result.failed;
        Ok(result.failed != 0)
    }

    /// Link issues for records a batch wrote, resolved against the batch's
    /// final state so an item may link to a record another item created. The
    /// collection is captured only when a written record requires a link.
    pub(crate) fn written_link_issues(
        &self,
        written: &[&crate::api::RecordDocument],
    ) -> Result<Vec<Issue>, crate::snapshot::SnapshotError> {
        let mut index = None;
        let mut issues = Vec::new();
        for record in written {
            let types = lowercase(&record.types);
            if self
                .required_link_values(&record.effective_frontmatter, &types)
                .is_empty()
            {
                continue;
            }
            if index.is_none() {
                let snapshot = self.capture_collection_snapshot_current()?;
                index = Some(snapshot.link_resolution_index(self));
            }
            issues.extend(self.check_link_exists(
                &record.effective_frontmatter,
                &types,
                record.path.as_str(),
                index.as_ref().expect("the index was built above"),
            ));
        }
        Ok(issues)
    }
}

fn duplicate_issue(path: &str, key: &UniqueSetKey, declaring_type: &str, message: String) -> Issue {
    Issue {
        code: DUPLICATE_VALUE.to_string(),
        message,
        path: Some(path.to_string()),
        field: Some(key.field.clone()),
        severity: Severity::Error,
        expected: None,
        actual: Some(Value::String(key.value.clone())),
        type_name: Some(declaring_type.to_string()),
        line: None,
        column: None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::UniqueCorpusEntry;
    use crate::runtime::{CatalogInput, CompiledCatalog, ResolvedTypeResource};

    fn catalog(rule: Value) -> CompiledCatalog {
        let type_resource = |name: &str, glob: &str, collection: Value| ResolvedTypeResource {
            path: format!("_types/{name}.md"),
            revision: "type-1".to_string(),
            definition: json!({
                "kind": "mdbase.type",
                "name": name,
                "version": 1,
                "match": {"path_glob": glob},
                "schema": {"dialect": "json-schema-2020-12", "value": {"type": "object"}},
                "collection": collection
            }),
            schema: json!({"type": "object"}),
        };
        CompiledCatalog::compile(CatalogInput {
            resource_revision: "catalog-scopes".to_string(),
            configuration_document: "spec_version: 0.3.0\n".to_string(),
            types: vec![
                type_resource("note", "notes/**/*.md", json!({"unique": [rule]})),
                type_resource("page", "pages/**/*.md", json!({})),
            ],
            contracts: Vec::new(),
        })
        .unwrap()
    }

    fn entry(path: &str, types: &[&str]) -> UniqueCorpusEntry {
        (
            path.to_string(),
            json!({"slug": "same"}),
            types.iter().map(|name| name.to_string()).collect(),
        )
    }

    fn conflicts(catalog: &CompiledCatalog, corpus: &[UniqueCorpusEntry]) -> Vec<String> {
        let mut paths = catalog
            .collection()
            .unique_value_issues(
                &json!({"slug": "same"}),
                &["note".to_string()],
                "notes/new.md",
                corpus,
            )
            .into_iter()
            .map(|issue| {
                issue
                    .message
                    .rsplit("also in ")
                    .next()
                    .unwrap()
                    .trim_end_matches(')')
                    .to_string()
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    #[test]
    fn unique_rules_compare_within_their_scope() {
        let corpus = vec![
            entry("notes/a.md", &["note"]),
            entry("pages/b.md", &["page"]),
            entry("notes/archive/c.md", &["note"]),
            entry("pages/archive/d.md", &["page"]),
        ];

        let omitted = catalog(json!({"field": "slug"}));
        assert_eq!(
            conflicts(&omitted, &corpus),
            ["notes/a.md", "notes/archive/c.md"]
        );
        let type_scope = catalog(json!({"field": "slug", "scope": "type"}));
        assert_eq!(
            conflicts(&type_scope, &corpus),
            ["notes/a.md", "notes/archive/c.md"]
        );

        let collection_scope = catalog(json!({"field": "slug", "scope": "collection"}));
        assert_eq!(conflicts(&collection_scope, &corpus).len(), 4);

        let glob_scope =
            catalog(json!({"field": "slug", "scope": "path_glob", "path_glob": "*/archive/**"}));
        assert_eq!(
            conflicts(&glob_scope, &corpus),
            ["notes/archive/c.md", "pages/archive/d.md"]
        );

        // Only records whose own types declare a rule must hold its value alone.
        assert!(collection_scope
            .collection()
            .unique_value_issues(
                &json!({"slug": "same"}),
                &["page".to_string()],
                "pages/new.md",
                &corpus
            )
            .is_empty());
    }

    #[test]
    fn corpus_issues_name_every_record_that_must_hold_a_shared_value() {
        let corpus = vec![
            entry("notes/a.md", &["note"]),
            entry("notes/b.md", &["note"]),
            entry("pages/c.md", &["page"]),
        ];
        let flagged = |catalog: &CompiledCatalog| {
            let mut paths = catalog
                .collection()
                .corpus_unique_value_issues(&corpus)
                .into_iter()
                .filter_map(|issue| issue.path)
                .collect::<Vec<_>>();
            paths.sort();
            paths
        };
        assert_eq!(
            flagged(&catalog(json!({"field": "slug"}))),
            ["notes/a.md", "notes/b.md"]
        );
        assert_eq!(
            flagged(&catalog(json!({"field": "slug", "scope": "collection"}))),
            ["notes/a.md", "notes/b.md"]
        );
        let single = vec![
            entry("notes/a.md", &["note"]),
            entry("pages/c.md", &["page"]),
        ];
        assert!(catalog(json!({"field": "slug"}))
            .collection()
            .corpus_unique_value_issues(&single)
            .is_empty());
        assert_eq!(
            catalog(json!({"field": "slug", "scope": "collection"}))
                .collection()
                .corpus_unique_value_issues(&single)
                .len(),
            1
        );
    }

    mod runtime_writes {
        use std::fs;
        use std::time::Duration;

        use serde_json::{json, Value};

        use crate::runtime::{
            FilesystemRuntime, HostClaimId, OperationContext, OperationKind, OperationRequest,
            PreparationOutcome,
        };

        const TYPES: &[(&str, &str)] = &[
            (
                "note",
                "---\nkind: mdbase.type\nname: note\nversion: 1\nmatch:\n  path_glob: \"notes/*.md\"\n\
                 schema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n\
                 collection:\n  unique:\n    - field: slug\n      scope: collection\n\
                 \x20 links:\n    related:\n      validate_exists: true\n---\n",
            ),
            (
                "page",
                "---\nkind: mdbase.type\nname: page\nversion: 1\nmatch:\n  path_glob: \"pages/*.md\"\n\
                 schema:\n  dialect: json-schema-2020-12\n  value:\n    type: object\n---\n",
            ),
        ];

        fn runtime() -> (tempfile::TempDir, FilesystemRuntime) {
            let directory = tempfile::tempdir().unwrap();
            fs::write(
                directory.path().join("mdbase.yaml"),
                "spec_version: 0.3.0\nsettings:\n  default_validation: error\n",
            )
            .unwrap();
            for folder in ["_types", "notes", "pages"] {
                fs::create_dir(directory.path().join(folder)).unwrap();
            }
            for (name, document) in TYPES {
                fs::write(directory.path().join(format!("_types/{name}.md")), document).unwrap();
            }
            fs::write(
                directory.path().join("pages/taken.md"),
                "---\nslug: taken\n---\n",
            )
            .unwrap();
            fs::write(
                directory.path().join("notes/existing.md"),
                "---\nslug: existing\n---\n",
            )
            .unwrap();
            let runtime =
                FilesystemRuntime::open(directory.path(), Duration::from_millis(5)).unwrap();
            (directory, runtime)
        }

        /// Diagnostic codes of a rejected write, or None when it was prepared.
        fn rejection(
            runtime: &FilesystemRuntime,
            kind: OperationKind,
            input: Value,
        ) -> Option<Vec<String>> {
            match runtime
                .prepare(
                    &OperationRequest::new(kind, input),
                    &HostClaimId::generate(),
                    &OperationContext::new(
                        &crate::cancellation::OperationCancellation::new(),
                        crate::runtime::OperationDeadline::after(Duration::from_secs(60)),
                    ),
                )
                .unwrap()
            {
                PreparationOutcome::NoMutation(outcome) if !outcome.operation.valid => {
                    let mut codes = outcome
                        .operation
                        .diagnostics
                        .iter()
                        .map(|diagnostic| diagnostic.code.as_str().to_string())
                        .collect::<Vec<_>>();
                    if let crate::runtime::CanonicalOperationValue::Batch(Some(batch)) =
                        &outcome.operation.value
                    {
                        codes.extend(batch.operations.iter().flat_map(|item| {
                            item.diagnostics.iter().map(|d| d.code.as_str().to_string())
                        }));
                    }
                    Some(codes)
                }
                _ => None,
            }
        }

        fn note(path: &str, frontmatter: Value) -> Value {
            json!({"path": path, "type": "note", "frontmatter": frontmatter})
        }

        #[test]
        fn runtime_writes_enforce_required_links_and_scoped_uniqueness() {
            let (_directory, runtime) = runtime();
            let create = |input| rejection(&runtime, OperationKind::Create, input);

            let missing = create(note("notes/a.md", json!({"related": "[[notes/missing]]"})));
            assert!(missing.unwrap().contains(&"link_not_found".to_string()));
            assert_eq!(
                create(note("notes/a.md", json!({"related": "[[notes/existing]]"}))),
                None
            );

            // `scope: collection` compares with the page even though pages
            // declare no rule; a page may still take a note's value.
            let taken = create(note("notes/a.md", json!({"slug": "taken"})));
            assert!(taken.unwrap().contains(&"duplicate_value".to_string()));
            assert_eq!(
                rejection(
                    &runtime,
                    OperationKind::Create,
                    json!({"path": "pages/b.md", "type": "page", "frontmatter": {"slug": "existing"}}),
                ),
                None
            );

            let update = rejection(
                &runtime,
                OperationKind::Update,
                json!({"path": "notes/existing.md", "patch": {"related": "[[pages/gone]]"}}),
            );
            assert!(update.unwrap().contains(&"link_not_found".to_string()));
        }

        #[test]
        fn batch_links_resolve_against_the_batch_final_state() {
            let (_directory, runtime) = runtime();
            let batch = |second_link: &str| {
                rejection(
                    &runtime,
                    OperationKind::Batch,
                    json!({"operations": [
                        {"kind": "create", "input": note("notes/target.md", json!({}))},
                        {"kind": "create", "input": note("notes/source.md", json!({"related": second_link}))}
                    ]}),
                )
            };
            assert_eq!(batch("[[notes/target]]"), None);
            assert!(batch("[[notes/missing]]")
                .unwrap()
                .contains(&"link_not_found".to_string()));
        }
    }
}
