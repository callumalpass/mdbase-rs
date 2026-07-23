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
        if let serde_yaml::Value::Mapping(map) = fm {
            let keys: Vec<serde_yaml::Value> = map.keys().cloned().collect();
            for key in keys {
                let key_str = key.as_str().map(|s| s.to_string()).unwrap_or_default();
                if let Some(val) = map.get_mut(&key) {
                    match val {
                        serde_yaml::Value::String(s) => {
                            let resolves =
                                self.link_resolves_to(s, from, from_stem, from_no_ext, source_dir);
                            if resolves {
                                // Check for id-stability: if the link resolves via id and id didn't change, skip
                                if self.should_skip_id_stable_link(
                                    s, source_id, from_stem, rel_path, &key_str,
                                ) {
                                    continue;
                                }
                                // Check for ambiguity
                                if self.is_ambiguous_link_with_counts(s, ambiguous_stem_counts) {
                                    warnings.push(serde_json::json!({
                                        "path": rel_path,
                                        "message": format!("Ambiguous link '{}' not updated", s),
                                    }));
                                    continue;
                                }
                                let new_val = self.rewrite_link_value(
                                    s,
                                    from_stem,
                                    to_stem,
                                    from_no_ext,
                                    to_no_ext,
                                    to,
                                    source_dir,
                                );
                                *s = new_val;
                                *changed = true;
                                refs_updated.push(serde_json::json!({
                                    "path": rel_path,
                                    "field": key_str,
                                }));
                            }
                        }
                        serde_yaml::Value::Sequence(items) => {
                            for (idx, item) in items.iter_mut().enumerate() {
                                if let serde_yaml::Value::String(s) = item {
                                    if self.link_resolves_to(
                                        s,
                                        from,
                                        from_stem,
                                        from_no_ext,
                                        source_dir,
                                    ) {
                                        if self.should_skip_id_stable_link(
                                            s, source_id, from_stem, rel_path, &key_str,
                                        ) {
                                            continue;
                                        }
                                        if self
                                            .is_ambiguous_link_with_counts(s, ambiguous_stem_counts)
                                        {
                                            warnings.push(serde_json::json!({
                                                "path": rel_path,
                                                "message": format!("Ambiguous link '{}' not updated", s),
                                            }));
                                            continue;
                                        }
                                        let new_val = self.rewrite_link_value(
                                            s,
                                            from_stem,
                                            to_stem,
                                            from_no_ext,
                                            to_no_ext,
                                            to,
                                            source_dir,
                                        );
                                        *s = new_val;
                                        *changed = true;
                                        refs_updated.push(serde_json::json!({
                                            "path": rel_path,
                                            "field": format!("{}[{}]", key_str, idx),
                                        }));
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
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
                if !target.contains('/') && !target.contains('.') && target == id.as_str() {
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
                *stem_counts.entry(stem.to_string()).or_insert(0) += 1;
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
                *stem_counts.entry(from_stem.to_string()).or_insert(0) += 1;
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

        stem_counts.get(&target).copied().unwrap_or(0) > 1
    }
}
