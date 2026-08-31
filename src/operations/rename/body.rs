use crate::links::resolver::compute_relative_path;
use crate::Collection;

impl Collection {
    /// Update body links (wikilinks and markdown links) to point to the new path.
    /// Returns true if any changes were made.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_body_links(
        &self,
        body: &mut String,
        from: &str,
        to: &str,
        from_stem: &str,
        to_stem: &str,
        from_no_ext: &str,
        to_no_ext: &str,
        source_dir: &str,
        source_path: &str,
        source_id: Option<&str>,
        resolution_index: &crate::links::resolver::LinkResolutionIndex,
    ) -> Result<bool, crate::runtime::CatalogError> {
        let mut changed = false;

        // Process line by line, skipping fenced code blocks and inline code
        let mut result = String::with_capacity(body.len());
        let mut in_fence = false;
        let mut fence_marker: Option<char> = None;
        let mut fence_count = 0;

        for line in body.split('\n') {
            let trimmed = line.trim_start();

            if !in_fence {
                // Check for opening fence
                if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                    let fc = trimmed.chars().next().unwrap();
                    let cnt = trimmed.chars().take_while(|&c| c == fc).count();
                    in_fence = true;
                    fence_marker = Some(fc);
                    fence_count = cnt;
                    result.push_str(line);
                    result.push('\n');
                    continue;
                }
                // Process this line for link replacements (outside code blocks)
                let new_line = self.replace_links_in_line(
                    line,
                    from,
                    to,
                    from_stem,
                    to_stem,
                    from_no_ext,
                    to_no_ext,
                    source_dir,
                    source_path,
                    source_id,
                    resolution_index,
                )?;
                if new_line != line {
                    changed = true;
                }
                result.push_str(&new_line);
                result.push('\n');
            } else {
                // Check for closing fence
                if let Some(fc) = fence_marker {
                    if trimmed.starts_with(fc) {
                        let cnt = trimmed.chars().take_while(|&c| c == fc).count();
                        if cnt >= fence_count && trimmed[cnt * fc.len_utf8()..].trim().is_empty() {
                            in_fence = false;
                        }
                    }
                }
                result.push_str(line);
                result.push('\n');
            }
        }

        // Remove trailing newline added by split/join
        if result.ends_with('\n') && !body.ends_with('\n') {
            result.pop();
        }
        // Handle case where body ends with \n but we added an extra
        if body.ends_with('\n') && result.ends_with("\n\n") && !body.ends_with("\n\n") {
            result.pop();
        }

        if changed {
            *body = result;
        }
        Ok(changed)
    }

    /// Replace link references in a single line (outside code blocks).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace_links_in_line(
        &self,
        line: &str,
        _from: &str,
        to: &str,
        from_stem: &str,
        to_stem: &str,
        from_no_ext: &str,
        to_no_ext: &str,
        source_dir: &str,
        source_path: &str,
        source_id: Option<&str>,
        resolution_index: &crate::links::resolver::LinkResolutionIndex,
    ) -> Result<String, crate::runtime::CatalogError> {
        let mut result = String::with_capacity(line.len());
        let chars: Vec<char> = line.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            // Skip inline code
            if chars[i] == '`' {
                let start = i;
                let bt_count = chars[i..].iter().take_while(|&&c| c == '`').count();
                i += bt_count;
                let mut found = false;
                while i + bt_count <= len {
                    if chars[i] == '`' {
                        let close_count = chars[i..].iter().take_while(|&&c| c == '`').count();
                        if close_count == bt_count {
                            for c in &chars[start..i + close_count] {
                                result.push(*c);
                            }
                            i += close_count;
                            found = true;
                            break;
                        }
                        i += close_count;
                    } else {
                        i += 1;
                    }
                }
                if !found {
                    for c in &chars[start..] {
                        result.push(*c);
                    }
                    break;
                }
                continue;
            }

            // Check for ![ (could be embed ![[...]] or markdown image ![alt](path))
            if chars[i] == '!' && i + 1 < len && chars[i + 1] == '[' {
                if i + 2 < len && chars[i + 2] == '[' {
                    // Wikilink embed: ![[target]]
                    let link_start = i;
                    i += 3; // skip ![[
                    let content_start = i;
                    while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                        i += 1;
                    }
                    if i < len {
                        let inner: String = chars[content_start..i].iter().collect();
                        i += 2; // skip ]]
                        if self.body_link_resolves_to(
                            &format!("[[{}]]", inner),
                            _from,
                            from_stem,
                            from_no_ext,
                            source_dir,
                            source_path,
                            source_id,
                            resolution_index,
                        )? {
                            let new_inner = self.rewrite_wikilink_inner(
                                &inner,
                                from_stem,
                                to_stem,
                                from_no_ext,
                                to_no_ext,
                            );
                            result.push_str(&format!("![[{}]]", new_inner));
                        } else {
                            for c in &chars[link_start..i] {
                                result.push(*c);
                            }
                        }
                        continue;
                    }
                    for c in &chars[link_start..len] {
                        result.push(*c);
                    }
                    break;
                } else {
                    // Markdown image: ![alt](path)
                    let link_start = i;
                    i += 2; // skip ![
                    let mut depth = 1;
                    while i < len && depth > 0 {
                        if chars[i] == '[' {
                            depth += 1;
                        }
                        if chars[i] == ']' {
                            depth -= 1;
                        }
                        i += 1;
                    }
                    if i < len && chars[i] == '(' {
                        let paren_start = i + 1;
                        i += 1;
                        let mut pdepth = 1;
                        while i < len && pdepth > 0 {
                            if chars[i] == '(' {
                                pdepth += 1;
                            }
                            if chars[i] == ')' {
                                pdepth -= 1;
                            }
                            i += 1;
                        }
                        let href: String = chars[paren_start..i - 1].iter().collect();
                        if !href.starts_with("http://")
                            && !href.starts_with("https://")
                            && self.body_link_resolves_to(
                                &href,
                                _from,
                                from_stem,
                                from_no_ext,
                                source_dir,
                                source_path,
                                source_id,
                                resolution_index,
                            )?
                        {
                            let text_part: String =
                                chars[link_start..paren_start - 1].iter().collect();
                            let (_, anchor) = if let Some(hp) = href.find('#') {
                                (&href[..hp], &href[hp..])
                            } else {
                                (href.as_str(), "")
                            };
                            let new_rel = compute_relative_path(source_dir, to);
                            result.push_str(&format!("{}({}{}", text_part, new_rel, anchor));
                            result.push(')');
                            continue;
                        }
                        for c in &chars[link_start..i] {
                            result.push(*c);
                        }
                        continue;
                    }
                    for c in &chars[link_start..i] {
                        result.push(*c);
                    }
                    continue;
                }
            }

            // Wikilink: [[target]]
            if i + 1 < len && chars[i] == '[' && chars[i + 1] == '[' {
                let link_start = i;
                i += 2;
                let content_start = i;
                while i < len && !(chars[i] == ']' && i + 1 < len && chars[i + 1] == ']') {
                    i += 1;
                }
                if i < len {
                    let inner: String = chars[content_start..i].iter().collect();
                    i += 2;
                    if self.body_link_resolves_to(
                        &format!("[[{}]]", inner),
                        _from,
                        from_stem,
                        from_no_ext,
                        source_dir,
                        source_path,
                        source_id,
                        resolution_index,
                    )? {
                        let new_inner = self.rewrite_wikilink_inner(
                            &inner,
                            from_stem,
                            to_stem,
                            from_no_ext,
                            to_no_ext,
                        );
                        result.push_str(&format!("[[{}]]", new_inner));
                    } else {
                        for c in &chars[link_start..i] {
                            result.push(*c);
                        }
                    }
                    continue;
                }
                for c in &chars[link_start..len] {
                    result.push(*c);
                }
                break;
            }

            // Markdown link: [text](path)
            if chars[i] == '[' {
                let link_start = i;
                i += 1;
                let mut depth = 1;
                while i < len && depth > 0 {
                    if chars[i] == '[' {
                        depth += 1;
                    }
                    if chars[i] == ']' {
                        depth -= 1;
                    }
                    i += 1;
                }
                if i < len && chars[i] == '(' {
                    let paren_start = i + 1;
                    i += 1;
                    let mut pdepth = 1;
                    while i < len && pdepth > 0 {
                        if chars[i] == '(' {
                            pdepth += 1;
                        }
                        if chars[i] == ')' {
                            pdepth -= 1;
                        }
                        i += 1;
                    }
                    let href: String = chars[paren_start..i - 1].iter().collect();
                    if !href.starts_with("http://")
                        && !href.starts_with("https://")
                        && self.body_link_resolves_to(
                            &href,
                            _from,
                            from_stem,
                            from_no_ext,
                            source_dir,
                            source_path,
                            source_id,
                            resolution_index,
                        )?
                    {
                        let text_part: String = chars[link_start..paren_start - 1].iter().collect();
                        let (_, anchor) = if let Some(hp) = href.find('#') {
                            (&href[..hp], &href[hp..])
                        } else {
                            (href.as_str(), "")
                        };
                        let new_rel = compute_relative_path(source_dir, to);
                        result.push_str(&format!("{}({}{}", text_part, new_rel, anchor));
                        result.push(')');
                        continue;
                    }
                    for c in &chars[link_start..i] {
                        result.push(*c);
                    }
                    continue;
                }
                for c in &chars[link_start..i] {
                    result.push(*c);
                }
                continue;
            }

            result.push(chars[i]);
            i += 1;
        }

        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn body_link_resolves_to(
        &self,
        link: &str,
        from: &str,
        from_stem: &str,
        from_no_ext: &str,
        source_dir: &str,
        source_path: &str,
        source_id: Option<&str>,
        resolution_index: &crate::links::resolver::LinkResolutionIndex,
    ) -> Result<bool, crate::runtime::CatalogError> {
        if self.is_stable_configured_id_wikilink(link, source_id) {
            return Ok(false);
        }
        if !self.link_resolves_to(link, from, from_stem, from_no_ext, source_dir) {
            return Ok(false);
        }
        match self.simple_wikilink_resolution(link, source_path, &[], resolution_index) {
            None => Ok(true),
            Some(Ok(crate::links::resolver::LinkResolution::Resolved { path, .. })) => {
                Ok(path == from)
            }
            Some(Ok(
                crate::links::resolver::LinkResolution::Missing
                | crate::links::resolver::LinkResolution::Ambiguous(_),
            )) => Ok(false),
            Some(Err(error)) => Err(error),
        }
    }
}

#[cfg(test)]
mod selector_failure_tests {
    use super::*;

    #[test]
    fn body_rewrite_propagates_selector_failure() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("mdbase.yaml"), "spec_version: 0.3.0\n").unwrap();
        let collection = Collection::open(root.path()).unwrap();
        let mut index = crate::links::resolver::LinkResolutionIndex::default();
        index
            .basename_lower_to_paths
            .insert("target".to_string(), vec!["target.md".to_string()]);
        let mut body = "[[target]]".to_string();

        let error = collection
            .update_body_links(
                &mut body,
                "target.md",
                "renamed.md",
                "target",
                "renamed",
                "target",
                "renamed",
                "",
                "../unsafe-source.md",
                None,
                &index,
            )
            .unwrap_err();

        assert_eq!(error.code, "invalid_resolution_candidate");
        assert_eq!(body, "[[target]]");
    }
}
