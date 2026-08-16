use std::collections::HashMap;

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
        ambiguous_stem_counts: &HashMap<String, usize>,
        changed: &mut bool,
        refs_updated: &mut Vec<serde_json::Value>,
        warnings: &mut Vec<serde_json::Value>,
    ) {
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
            ambiguous_stem_counts,
            changed,
            refs_updated,
            warnings,
        }
        .visit(fm, "");
    }

    /// Check if a wikilink resolves via id_field and the id didn't change (id-stability).
    /// Per spec §12.5: implementations SHOULD NOT rewrite the link when the id_field
    /// value hasn't changed, to avoid unnecessary churn.
    /// We apply this only when the link target matches the id AND the id differs
    /// from the old filename stem (so the link was genuinely id-based, not filename-based).
    pub(crate) fn should_skip_id_stable_link(
        &self,
        link_val: &str,
        source_id: &Option<String>,
        _from_stem: &str,
        source_file_path: &str,
        field_name: &str,
    ) -> bool {
        if !self.settings.id_field_explicit {
            return false;
        }
        if let Some(id) = source_id {
            // Only wikilinks can resolve via id_field. Markdown links and bare paths
            // resolve by path and always need updating.
            if link_val.starts_with("[[") && link_val.ends_with("]]") {
                let inner = &link_val[2..link_val.len() - 2];
                let target = inner
                    .split('|')
                    .next()
                    .unwrap_or(inner)
                    .split('#')
                    .next()
                    .unwrap_or(inner)
                    .trim();
                // Simple name (no path separators or extensions) that matches the
                // renamed file's id_field value -> potentially id-stable
                if !target.contains('/') && !target.contains('.') && target.eq_ignore_ascii_case(id)
                {
                    // Only skip if the link field has a typed target constraint,
                    // meaning it resolves via id lookup rather than filename.
                    // Generic link fields (no target type) resolve by filename
                    // and must be updated.
                    if !self
                        .get_field_target_types(source_file_path, field_name)
                        .is_empty()
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Check if a link was ambiguous before the rename (matched multiple files).
    /// Executed renames account for the old path, while dry runs still see it on disk.
    pub(crate) fn collect_wikilink_stem_counts(
        &self,
        from_path: &str,
        source_already_renamed: bool,
    ) -> HashMap<String, usize> {
        let mut stem_counts: HashMap<String, usize> = HashMap::new();
        for file_path in self.scan_collection_files() {
            let rel_path = match file_path.strip_prefix(&self.root) {
                Ok(p) => p.to_string_lossy().to_string().replace('\\', "/"),
                Err(_) => continue,
            };
            let stem = std::path::Path::new(&rel_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !stem.is_empty() {
                *stem_counts.entry(stem.to_ascii_lowercase()).or_insert(0) += 1;
            }
        }

        if source_already_renamed {
            // The source no longer exists on disk, but its old stem participated
            // in resolution immediately before the rename.
            let from_stem = std::path::Path::new(from_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if !from_stem.is_empty() {
                *stem_counts
                    .entry(from_stem.to_ascii_lowercase())
                    .or_insert(0) += 1;
            }
        }

        stem_counts
    }

    pub(crate) fn is_ambiguous_link_with_counts(
        &self,
        link_val: &str,
        stem_counts: &HashMap<String, usize>,
    ) -> bool {
        let target = if link_val.starts_with("[[") && link_val.ends_with("]]") {
            let inner = &link_val[2..link_val.len() - 2];
            inner
                .split('|')
                .next()
                .unwrap_or(inner)
                .split('#')
                .next()
                .unwrap_or(inner)
                .trim()
                .to_string()
        } else {
            return false; // Only wikilinks can be ambiguous
        };

        if target.is_empty() || target.contains('/') || target.contains('.') {
            return false; // Path-based links are not ambiguous
        }

        stem_counts
            .get(&target.to_ascii_lowercase())
            .copied()
            .unwrap_or(0)
            > 1
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
    ambiguous_stem_counts: &'a HashMap<String, usize>,
    changed: &'a mut bool,
    refs_updated: &'a mut Vec<serde_json::Value>,
    warnings: &'a mut Vec<serde_json::Value>,
}

impl FrontmatterRewriteContext<'_> {
    fn visit(&mut self, value: &mut serde_yaml::Value, field: &str) {
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
                    self.visit(child, &child_field);
                }
            }
            serde_yaml::Value::Sequence(items) => {
                for (index, item) in items.iter_mut().enumerate() {
                    self.visit(item, &format!("{field}[{index}]"));
                }
            }
            serde_yaml::Value::String(link) => self.rewrite_string(link, field),
            _ => {}
        }
    }

    fn rewrite_string(&mut self, link: &mut String, field: &str) {
        if self.collection.link_resolves_to(
            link,
            self.from,
            self.from_stem,
            self.from_no_ext,
            self.source_dir,
            self.source_id.as_deref(),
        ) {
            if self.collection.should_skip_id_stable_link(
                link,
                self.source_id,
                self.from_stem,
                self.rel_path,
                field,
            ) {
                return;
            }
            if self
                .collection
                .is_ambiguous_link_with_counts(link, self.ambiguous_stem_counts)
            {
                self.warnings.push(serde_json::json!({
                    "path": self.rel_path,
                    "message": format!("Ambiguous link '{}' not updated", link),
                }));
                return;
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
            return;
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
            self.source_id.as_deref(),
        );
        if rewritten != *link {
            *link = rewritten;
            self.record_change(field);
        }
    }

    fn record_change(&mut self, field: &str) {
        *self.changed = true;
        self.refs_updated.push(serde_json::json!({
            "path": self.rel_path,
            "field": field,
        }));
    }
}
