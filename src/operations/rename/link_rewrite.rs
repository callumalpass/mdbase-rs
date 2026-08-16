use crate::links::parser::normalize_link_path;
use crate::links::resolver::compute_relative_path;
use crate::Collection;

impl Collection {
    /// Check if a link value resolves to the renamed file.
    pub(crate) fn link_resolves_to(
        &self,
        link_val: &str,
        from_path: &str,
        from_stem: &str,
        from_no_ext: &str,
        source_dir: &str,
        source_id: Option<&str>,
    ) -> bool {
        let is_wikilink = link_val.starts_with("[[") && link_val.ends_with("]]");
        let is_md_link = link_val.contains("](");
        let bare_target = link_val.split('#').next().unwrap_or(link_val).trim();
        let is_bare_path = link_val.starts_with("./")
            || link_val.starts_with("../")
            || link_val.contains('/')
            || bare_target.ends_with(".md")
            || bare_target.ends_with(".mdx");

        // Only process actual link-formatted values
        if !is_wikilink && !is_md_link && !is_bare_path {
            return false;
        }

        // Strip wikilink syntax
        let target = if is_wikilink {
            let inner = &link_val[2..link_val.len() - 2];
            inner
                .split('|')
                .next()
                .unwrap_or(inner)
                .split('#')
                .next()
                .unwrap_or(inner)
                .trim()
        } else if is_md_link {
            // Markdown link: extract path from [text](path)
            if let Some(start) = link_val.find("](") {
                let rest = &link_val[start + 2..];
                let end = rest.find(')').unwrap_or(rest.len());
                rest[..end].split('#').next().unwrap_or(&rest[..end]).trim()
            } else {
                return false;
            }
        } else {
            // Bare path
            link_val.split('#').next().unwrap_or(link_val).trim()
        };

        if target.is_empty() {
            return false;
        }

        // A simple wikilink matching the configured record ID continues to
        // resolve after a path-only rename.  Treat it as stable even when its
        // spelling differs only by case from the former filename stem.
        if is_wikilink
            && !target.contains('/')
            && !target.contains('.')
            && target != from_stem
            && source_id.is_some_and(|id| target.eq_ignore_ascii_case(id))
        {
            return false;
        }

        // Normalize the target relative to source file
        let normalized = normalize_link_path(target, source_dir);

        // Check if it resolves to the from path
        let norm_with_md = if !normalized.ends_with(".md") && !normalized.ends_with(".mdx") {
            format!("{}.md", normalized)
        } else {
            normalized.clone()
        };

        if norm_with_md == from_path || normalized == from_path || normalized == from_no_ext {
            return true;
        }

        // Check stem match for simple wikilinks (only wikilinks can match by stem)
        if is_wikilink
            && !target.contains('/')
            && !target.contains('.')
            && target.eq_ignore_ascii_case(from_stem)
        {
            return true;
        }

        false
    }

    /// Rewrite a link value to point to the new path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rewrite_link_value(
        &self,
        link_val: &str,
        from_stem: &str,
        to_stem: &str,
        _from_no_ext: &str,
        to_no_ext: &str,
        to_path: &str,
        source_dir: &str,
    ) -> String {
        if link_val.starts_with("[[") && link_val.ends_with("]]") {
            // Wikilink: [[target]], [[target|alias]], [[target#anchor]]
            let inner = &link_val[2..link_val.len() - 2];
            let (target_part, rest) = if let Some(pipe_pos) = inner.find('|') {
                (&inner[..pipe_pos], &inner[pipe_pos..])
            } else {
                (inner, "")
            };
            let (name_part, anchor) = if let Some(hash_pos) = target_part.find('#') {
                (&target_part[..hash_pos], &target_part[hash_pos..])
            } else {
                (target_part, "")
            };

            // Determine new name
            let new_name = if name_part.eq_ignore_ascii_case(from_stem)
                || name_part.trim().eq_ignore_ascii_case(from_stem)
            {
                // Simple name -> use new stem
                to_stem.to_string()
            } else if name_part.contains('/') {
                // Path-based wikilink -> use new path without extension
                to_no_ext.to_string()
            } else {
                to_stem.to_string()
            };

            format!("[[{}{}{}]]", new_name, anchor, rest)
        } else if link_val.contains("](") {
            // Markdown link: [text](path) or ![alt](path)
            let prefix_end = link_val.find("](").unwrap();
            let prefix = &link_val[..prefix_end + 2]; // includes "]("
            let rest_start = prefix_end + 2;
            let rest = &link_val[rest_start..];
            let paren_end = rest.rfind(')').unwrap_or(rest.len());
            let path_and_anchor = &rest[..paren_end];
            let suffix = &rest[paren_end..]; // the closing ")"

            let (_path_part, anchor) = if let Some(hash_pos) = path_and_anchor.find('#') {
                (&path_and_anchor[..hash_pos], &path_and_anchor[hash_pos..])
            } else {
                (path_and_anchor, "")
            };

            // Compute new relative path from source_dir to to_path
            let new_rel = compute_relative_path(source_dir, to_path);
            format!("{}{}{}{}", prefix, new_rel, anchor, suffix)
        } else {
            // Bare path
            let (_path_part, anchor) = if let Some(hash_pos) = link_val.find('#') {
                (&link_val[..hash_pos], &link_val[hash_pos..])
            } else {
                (link_val, "")
            };
            let new_rel = compute_relative_path(source_dir, to_path);
            format!("{}{}", new_rel, anchor)
        }
    }

    /// Rewrite the inner content of a wikilink (without the [[ ]] brackets).
    pub(crate) fn rewrite_wikilink_inner(
        &self,
        inner: &str,
        _from_stem: &str,
        to_stem: &str,
        _from_no_ext: &str,
        to_no_ext: &str,
    ) -> String {
        let (target_part, rest) = if let Some(pipe_pos) = inner.find('|') {
            (&inner[..pipe_pos], &inner[pipe_pos..])
        } else {
            (inner, "")
        };
        let (name_part, anchor) = if let Some(hash_pos) = target_part.find('#') {
            (&target_part[..hash_pos], &target_part[hash_pos..])
        } else {
            (target_part, "")
        };
        let new_name = if name_part.contains('/') {
            to_no_ext.to_string()
        } else {
            to_stem.to_string()
        };
        format!("{}{}{}", new_name, anchor, rest)
    }
}
