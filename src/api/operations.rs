#[cfg(feature = "legacy-collection-mutation")]
use std::collections::HashMap;

use crate::errors::*;

#[derive(Debug, Clone)]
pub struct ReadInput {
    pub path: String,
    pub include_document: bool,
}

impl ReadInput {
    pub fn parse(input: &serde_json::Value) -> Result<Self, serde_json::Value> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| op_error(INVALID_PATH, "path is required"))?;
        Ok(Self {
            path: path.to_string(),
            include_document: input
                .get("include_document")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        })
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct CreateInput {
    pub type_name: Option<String>,
    pub frontmatter: serde_json::Value,
    pub body: String,
    pub path: Option<String>,
    pub if_revision: Option<String>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl CreateInput {
    pub fn parse(input: &serde_json::Value) -> Self {
        #[cfg(test)]
        crate::mutation::probe_legacy_parse();
        let type_name = input
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let frontmatter = input
            .get("frontmatter")
            .or_else(|| input.get("fields"))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let body = input
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let if_revision = input
            .get("if_revision")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        Self {
            type_name,
            frontmatter,
            body,
            path,
            if_revision,
        }
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct UpdateInput {
    pub path: String,
    pub fields: serde_json::Value,
    pub body: Option<String>,
    pub document: Option<String>,
    pub last_known_mtime: Option<u64>,
    pub if_revision: Option<String>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl UpdateInput {
    pub fn parse(input: &serde_json::Value) -> Result<Self, serde_json::Value> {
        #[cfg(test)]
        crate::mutation::probe_legacy_parse();
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| op_error(INVALID_PATH, "path is required"))?;
        let has_fields = input.get("patch").is_some()
            || input.get("fields").is_some()
            || input.get("frontmatter").is_some();
        let fields = input
            .get("patch")
            .or_else(|| input.get("fields"))
            .or_else(|| input.get("frontmatter"))
            .cloned()
            .unwrap_or(serde_json::json!({}));
        let body = input
            .get("body")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let document = match input.get("document") {
            Some(serde_json::Value::String(document)) => Some(document.clone()),
            Some(_) => {
                return Err(op_error(
                    INVALID_REQUEST,
                    "document must be a string when provided",
                ))
            }
            None => None,
        };
        if document.is_some() && (has_fields || body.is_some()) {
            return Err(op_error(
                INVALID_REQUEST,
                "document cannot be combined with patch, fields, frontmatter, or body",
            ));
        }
        let last_known_mtime = input.get("last_known_mtime").and_then(|v| v.as_u64());
        let if_revision = input
            .get("if_revision")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        Ok(Self {
            path: path.to_string(),
            fields,
            body,
            document,
            last_known_mtime,
            if_revision,
        })
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct DeleteInput {
    pub path: String,
    pub check_backlinks: bool,
    pub dry_run: bool,
    pub last_known_mtime: Option<u64>,
    pub if_revision: Option<String>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl DeleteInput {
    pub fn parse(input: &serde_json::Value) -> Result<Self, serde_json::Value> {
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| op_error(INVALID_PATH, "path is required"))?;
        let check_backlinks = input
            .get("check_backlinks")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let dry_run = input
            .get("dry_run")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let last_known_mtime = input.get("last_known_mtime").and_then(|v| v.as_u64());
        let if_revision = input
            .get("if_revision")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        Ok(Self {
            path: path.to_string(),
            check_backlinks,
            dry_run,
            last_known_mtime,
            if_revision,
        })
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct SimulatedRefWrite {
    pub path: String,
    pub content: String,
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct RenameInput {
    pub from: String,
    pub to: String,
    pub update_refs: Option<bool>,
    pub dry_run: bool,
    pub last_known_mtime: Option<u64>,
    pub if_revision: Option<String>,
    pub simulate_before_ref_update: Vec<SimulatedRefWrite>,
    pub last_known_ref_mtimes: HashMap<String, u64>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl RenameInput {
    pub fn parse(input: &serde_json::Value) -> Result<Self, serde_json::Value> {
        let from = input
            .get("from")
            .or_else(|| input.get("path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| op_error(PATH_REQUIRED, "'from' is required"))?;
        let to = input
            .get("to")
            .or_else(|| input.get("new_path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| op_error(PATH_REQUIRED, "'to' is required"))?;

        let simulate_before_ref_update = input
            .get("simulate_before_ref_update")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|entry| {
                        let path = entry.get("path").and_then(|v| v.as_str())?;
                        let content = entry.get("content").and_then(|v| v.as_str())?;
                        Some(SimulatedRefWrite {
                            path: path.to_string(),
                            content: content.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let last_known_ref_mtimes = input
            .get("last_known_ref_mtimes")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|ms| (k.clone(), ms)))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        Ok(Self {
            from: from.to_string(),
            to: to.to_string(),
            update_refs: input.get("update_refs").and_then(|v| v.as_bool()),
            dry_run: input
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            last_known_mtime: input.get("last_known_mtime").and_then(|v| v.as_u64()),
            if_revision: input
                .get("if_revision")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            simulate_before_ref_update,
            last_known_ref_mtimes,
        })
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct CreateOutput {
    pub path: String,
    pub types: Vec<String>,
    pub frontmatter: serde_json::Value,
    pub body: String,
    pub valid: bool,
    pub warnings: Vec<serde_json::Value>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl CreateOutput {
    pub fn into_json(self) -> serde_json::Value {
        let mut result = serde_json::json!({
            "path": self.path,
            "types": self.types,
            "frontmatter": self.frontmatter,
            "body": self.body,
            "valid": self.valid,
        });
        if !self.warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(self.warnings);
        }
        result
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct UpdateOutput {
    pub path: String,
    pub frontmatter: serde_json::Value,
    pub body: String,
    pub warnings: Vec<serde_json::Value>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl UpdateOutput {
    pub fn into_json(self) -> serde_json::Value {
        let mut result = serde_json::json!({
            "path": self.path,
            "frontmatter": self.frontmatter,
            "body": self.body,
        });
        if !self.warnings.is_empty() {
            result["warnings"] = serde_json::Value::Array(self.warnings);
        }
        result
    }
}

#[cfg(feature = "legacy-collection-mutation")]
#[derive(Debug, Clone)]
pub struct DeleteOutput {
    pub path: String,
    pub deleted: bool,
    pub dry_run: bool,
    pub broken_links: Vec<serde_json::Value>,
}

#[cfg(feature = "legacy-collection-mutation")]
impl DeleteOutput {
    pub fn into_json(self) -> serde_json::Value {
        let mut result = serde_json::json!({
            "path": self.path,
            "deleted": self.deleted,
        });
        if self.dry_run {
            result["dry_run"] = serde_json::Value::Bool(true);
            result["would_delete"] = serde_json::Value::Bool(true);
        }
        if !self.broken_links.is_empty() {
            result["broken_links"] = serde_json::Value::Array(self.broken_links);
        }
        result
    }
}
