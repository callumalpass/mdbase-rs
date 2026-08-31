//! asFile() traversal (§8.7).

use crate::frontmatter::parser::{parse_document, yaml_mapping_to_json};
use crate::Collection;
use std::collections::HashMap;
use std::time::Instant;

#[derive(Debug, Clone, Default)]
pub(crate) struct BacklinksPerf {
    pub total_ms: f64,
    pub frontmatter_extract_ms: f64,
    pub body_links_extract_ms: f64,
    pub body_embeds_extract_ms: f64,
    pub resolve_targets_ms: f64,
    pub files_processed: usize,
    pub targets_scanned: usize,
    pub resolve_calls: usize,
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

impl Collection {
    /// Build all files data for asFile() traversal in expressions.
    pub fn build_all_files_data(&self) -> Vec<crate::expressions::evaluator::ResolvedFileData> {
        let files = self.scan_collection_files();
        files
            .iter()
            .filter_map(|fp| {
                let rp = fp
                    .strip_prefix(&self.root)
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
    pub fn build_backlinks_index(
        &self,
        all_files: &[crate::expressions::evaluator::ResolvedFileData],
    ) -> HashMap<String, Vec<String>> {
        self.build_backlinks_index_profiled(all_files, false).0
    }

    pub(crate) fn build_backlinks_index_profiled(
        &self,
        all_files: &[crate::expressions::evaluator::ResolvedFileData],
        profile: bool,
    ) -> (HashMap<String, Vec<String>>, Option<BacklinksPerf>) {
        use crate::expressions::evaluator::{
            extract_embeds_from_body, extract_links_from_body, extract_links_from_fm_value,
        };

        let total_start = Instant::now();
        let mut perf = BacklinksPerf::default();
        let mut index: HashMap<String, Vec<String>> = HashMap::new();
        let resolution_index = self.build_link_resolution_index(all_files);

        for file_data in all_files {
            perf.files_processed += 1;
            let source_path = &file_data.path;
            if crate::api::CollectionPath::new(source_path).is_err() {
                continue;
            }
            let mut targets: Vec<(String, Vec<String>)> = Vec::new();

            // Extract links from frontmatter values
            let frontmatter_start = Instant::now();
            if let serde_json::Value::Object(ref map) = file_data.frontmatter {
                for (key, val) in map {
                    let mut field_targets = Vec::new();
                    extract_links_from_fm_value(val, &mut field_targets);
                    let target_types = self.get_field_target_types_from_frontmatter(
                        source_path,
                        key,
                        &file_data.frontmatter,
                    );
                    targets.extend(
                        field_targets
                            .into_iter()
                            .map(|target| (target, target_types.clone())),
                    );
                }
            }
            perf.frontmatter_extract_ms += elapsed_ms(frontmatter_start);

            // Extract links from body
            let body_links_start = Instant::now();
            let body_links = extract_links_from_body(&file_data.body);
            targets.extend(body_links.into_iter().map(|target| (target, Vec::new())));
            perf.body_links_extract_ms += elapsed_ms(body_links_start);

            // Extract embeds from body
            let body_embeds_start = Instant::now();
            let body_embeds = extract_embeds_from_body(&file_data.body);
            targets.extend(body_embeds.into_iter().map(|target| (target, Vec::new())));
            perf.body_embeds_extract_ms += elapsed_ms(body_embeds_start);

            // Resolve each target and add to backlinks index
            let mut seen_targets: Vec<String> = Vec::new();
            perf.targets_scanned += targets.len();
            let resolve_start = Instant::now();
            for (target, target_types) in &targets {
                // Resolve the target to a file path
                perf.resolve_calls += 1;
                let resolved =
                    self.resolve_link_target(target, source_path, target_types, &resolution_index);
                if let Some(resolved_path) = resolved {
                    if !seen_targets.contains(&resolved_path) {
                        seen_targets.push(resolved_path.clone());
                        index
                            .entry(resolved_path)
                            .or_default()
                            .push(source_path.clone());
                    }
                }
            }
            perf.resolve_targets_ms += elapsed_ms(resolve_start);
        }

        // Deduplicate source entries per target
        for sources in index.values_mut() {
            sources.sort();
            sources.dedup();
        }

        perf.total_ms = elapsed_ms(total_start);
        if profile {
            (index, Some(perf))
        } else {
            (index, None)
        }
    }
}
