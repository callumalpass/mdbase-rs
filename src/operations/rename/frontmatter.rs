use crate::Collection;

impl Collection {
    /// Update frontmatter link fields to point to the new path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_fm_links(
        &self,
        fm: &mut serde_yaml::Value,
        from: &str,
        to: &str,
        from_stem: &str,
        to_stem: &str,
        from_no_ext: &str,
        to_no_ext: &str,
        source_dir: &str,
        rel_path: &str,
        source_id: &Option<String>,
        type_names: &[String],
        resolution_index: &crate::links::resolver::LinkResolutionIndex,
        changed: &mut bool,
        refs_updated: &mut Vec<serde_json::Value>,
        warnings: &mut Vec<serde_json::Value>,
    ) -> Result<(), crate::runtime::CatalogError> {
        let typed_target_fields = type_names
            .iter()
            .filter_map(|type_name| self.types.get(type_name))
            .flat_map(|type_def| type_def.fields.iter())
            .filter_map(|(name, field)| {
                let target_types = crate::links::resolver::allowed_target_types(field);
                (!target_types.is_empty()).then(|| (name.clone(), target_types))
            })
            .collect::<std::collections::HashMap<_, _>>();
        FrontmatterRewriteContext {
            collection: self,
            from,
            to,
            from_stem,
            to_stem,
            from_no_ext,
            to_no_ext,
            source_dir,
            rel_path,
            source_id,
            typed_target_fields: &typed_target_fields,
            resolution_index,
            changed,
            refs_updated,
            warnings,
        }
        .visit(fm, "")
    }
}

struct FrontmatterRewriteContext<'a> {
    collection: &'a Collection,
    from: &'a str,
    to: &'a str,
    from_stem: &'a str,
    to_stem: &'a str,
    from_no_ext: &'a str,
    to_no_ext: &'a str,
    source_dir: &'a str,
    rel_path: &'a str,
    source_id: &'a Option<String>,
    typed_target_fields: &'a std::collections::HashMap<String, Vec<String>>,
    resolution_index: &'a crate::links::resolver::LinkResolutionIndex,
    changed: &'a mut bool,
    refs_updated: &'a mut Vec<serde_json::Value>,
    warnings: &'a mut Vec<serde_json::Value>,
}

impl FrontmatterRewriteContext<'_> {
    fn visit(
        &mut self,
        value: &mut serde_yaml::Value,
        field: &str,
    ) -> Result<(), crate::runtime::CatalogError> {
        match value {
            serde_yaml::Value::Mapping(mapping) => {
                let keys = mapping.keys().cloned().collect::<Vec<_>>();
                for key in keys {
                    let Some(child) = mapping.get_mut(&key) else {
                        continue;
                    };
                    let key = key.as_str().unwrap_or_default();
                    let child_field = if field.is_empty() {
                        key.to_string()
                    } else {
                        format!("{field}.{key}")
                    };
                    self.visit(child, &child_field)?;
                }
            }
            serde_yaml::Value::Sequence(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    self.visit(item, &format!("{field}[{index}]"))?;
                }
            }
            serde_yaml::Value::Tagged(tagged) => self.visit(&mut tagged.value, field)?,
            serde_yaml::Value::String(link) => self.rewrite_string(link, field)?,
            _ => {}
        }
        Ok(())
    }

    fn rewrite_string(
        &mut self,
        link: &mut String,
        field: &str,
    ) -> Result<(), crate::runtime::CatalogError> {
        let target_types = self
            .typed_target_fields
            .get(field)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if self
            .collection
            .is_stable_configured_id_wikilink(link, self.source_id.as_deref())
        {
            return Ok(());
        }
        if self.collection.link_resolves_to(
            link,
            self.from,
            self.from_stem,
            self.from_no_ext,
            self.source_dir,
        ) {
            match self.collection.simple_wikilink_resolution(
                link,
                self.rel_path,
                target_types,
                self.resolution_index,
            ) {
                None => {}
                Some(Ok(crate::links::resolver::LinkResolution::Resolved { path, .. }))
                    if path == self.from => {}
                Some(Ok(crate::links::resolver::LinkResolution::Ambiguous(_))) => {
                    self.warnings.push(serde_json::json!({
                        "path": self.rel_path,
                        "message": format!("Ambiguous link '{}' not updated", link),
                    }));
                    return Ok(());
                }
                Some(Err(error)) => return Err(error),
                Some(Ok(
                    crate::links::resolver::LinkResolution::Missing
                    | crate::links::resolver::LinkResolution::Resolved { .. },
                )) => return Ok(()),
            }
            *link = self.collection.rewrite_link_value(
                link,
                self.from_stem,
                self.to_stem,
                self.from_no_ext,
                self.to_no_ext,
                self.to,
                self.source_dir,
            );
            self.record_change(field);
            return Ok(());
        }

        // A frontmatter scalar may contain prose with multiple links. Reuse the
        // same syntax-aware scanner as body rewriting, while recursive traversal
        // above covers nested mappings and arrays.
        let rewritten = self.collection.replace_links_in_line(
            link,
            self.from,
            self.to,
            self.from_stem,
            self.to_stem,
            self.from_no_ext,
            self.to_no_ext,
            self.source_dir,
            self.rel_path,
            self.source_id.as_deref(),
            self.resolution_index,
        )?;
        if rewritten != *link {
            *link = rewritten;
            self.record_change(field);
        }
        Ok(())
    }

    fn record_change(&mut self, field: &str) {
        *self.changed = true;
        self.refs_updated.push(serde_json::json!({
            "path": self.rel_path,
            "field": field,
        }));
    }
}

#[cfg(test)]
mod selector_failure_tests {
    use super::*;

    #[test]
    fn frontmatter_rewrite_propagates_selector_failure() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let mut index = crate::links::resolver::LinkResolutionIndex::default();
        index
            .basename_lower_to_paths
            .insert("target".to_string(), vec!["target.md".to_string()]);
        let mut frontmatter = serde_yaml::from_str("ref: '[[target]]'\n").unwrap();
        let mut changed = false;
        let mut updates = Vec::new();
        let mut warnings = Vec::new();

        let error = collection
            .update_fm_links(
                &mut frontmatter,
                "target.md",
                "renamed.md",
                "target",
                "renamed",
                "target",
                "renamed",
                "",
                "../unsafe-source.md",
                &None,
                &[],
                &index,
                &mut changed,
                &mut updates,
                &mut warnings,
            )
            .unwrap_err();

        assert_eq!(error.code, "invalid_resolution_candidate");
        assert!(!changed);
        assert!(updates.is_empty());
        assert!(warnings.is_empty());
    }
}
