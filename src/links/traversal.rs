//! asFile() traversal (§8.7).

use std::collections::HashMap;
use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::Collection;

impl Collection {
    /// Build all files data for asFile() traversal in expressions.
    pub fn build_all_files_data(&self) -> Vec<crate::expressions::evaluator::ResolvedFileData> {
        let files = self.scan_collection_files();
        files.iter()
            .filter_map(|fp| {
                let rp = fp.strip_prefix(&self.root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .replace('\\', "/");
                let content = std::fs::read_to_string(fp).ok()?;
                let doc = parse_document(&content);
                let fm = match &doc.frontmatter {
                    Some(serde_yaml::Value::Mapping(m)) => yaml_mapping_to_json(m),
                    _ => serde_json::json!({}),
                };
                let type_names = self.determine_types_for_path(&fm, Some(&rp));
                let effective = self.apply_defaults(&fm, &type_names);
                let effective = self.coerce_types(&effective, &type_names);
                Some(crate::expressions::evaluator::ResolvedFileData {
                    path: rp,
                    frontmatter: effective,
                    body: doc.body,
                })
            })
            .collect()
    }

    /// Build backlinks index from all files data.
    /// Returns a map: target_path -> Vec<source_path> (deduplicated).
    pub fn build_backlinks_index(&self, all_files: &[crate::expressions::evaluator::ResolvedFileData]) -> HashMap<String, Vec<String>> {
        use crate::expressions::evaluator::{extract_links_from_body, extract_embeds_from_body, extract_links_from_fm_value};

        let mut index: HashMap<String, Vec<String>> = HashMap::new();

        // Collect all known file paths for resolution
        let known_paths: Vec<&str> = all_files.iter().map(|f| f.path.as_str()).collect();

        for file_data in all_files {
            let source_path = &file_data.path;
            let mut targets: Vec<String> = Vec::new();

            // Extract links from frontmatter values
            if let serde_json::Value::Object(ref map) = file_data.frontmatter {
                for (_key, val) in map {
                    extract_links_from_fm_value(val, &mut targets);
                }
            }

            // Extract links from body
            let body_links = extract_links_from_body(&file_data.body);
            targets.extend(body_links);

            // Extract embeds from body
            let body_embeds = extract_embeds_from_body(&file_data.body);
            targets.extend(body_embeds);

            // Resolve each target and add to backlinks index
            let mut seen_targets: Vec<String> = Vec::new();
            for target in &targets {
                // Resolve the target to a file path
                let resolved = self.resolve_link_target(target, source_path, &known_paths);
                if let Some(resolved_path) = resolved {
                    if !seen_targets.contains(&resolved_path) {
                        seen_targets.push(resolved_path.clone());
                        index.entry(resolved_path)
                            .or_insert_with(Vec::new)
                            .push(source_path.clone());
                    }
                }
            }
        }

        // Deduplicate source entries per target
        for sources in index.values_mut() {
            sources.sort();
            sources.dedup();
        }

        index
    }
}
