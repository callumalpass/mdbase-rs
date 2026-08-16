use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ObsidianBaseDocument {
    #[serde(default)]
    pub filters: Option<BaseFilter>,
    #[serde(default)]
    pub formulas: BTreeMap<String, String>,
    #[serde(default)]
    pub properties: BTreeMap<String, Value>,
    #[serde(default)]
    pub views: Vec<ObsidianBaseView>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum BaseFilter {
    Expression(String),
    Logical(BaseLogicalFilter),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct BaseLogicalFilter {
    #[serde(default)]
    pub and: Option<BaseFilterList>,
    #[serde(default)]
    pub or: Option<BaseFilterList>,
    #[serde(default)]
    pub not: Option<BaseFilterList>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum BaseFilterList {
    Many(Vec<BaseFilter>),
    One(Box<BaseFilter>),
}

impl BaseFilterList {
    pub fn values(&self) -> Vec<&BaseFilter> {
        match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values.iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ObsidianBaseView {
    pub name: String,
    #[serde(rename = "type")]
    pub renderer: String,
    #[serde(default)]
    pub filters: Option<BaseFilter>,
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub sort: Vec<BaseSort>,
    #[serde(default, rename = "groupBy")]
    pub group_by: Option<BaseGroupBy>,
    pub limit: Option<u64>,
    #[serde(default)]
    pub options: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BaseSort {
    pub property: String,
    #[serde(default)]
    pub direction: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum BaseGroupBy {
    Property(String),
    Definition(BaseGroupByDefinition),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BaseGroupByDefinition {
    pub property: String,
    #[serde(default)]
    pub direction: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ViewReferenceInput {
    pub path: String,
    #[serde(rename = "view", alias = "view_id")]
    pub view_id: String,
    pub timezone: Option<String>,
    #[serde(default)]
    pub context: Option<Option<ViewContextInput>>,
    pub limit: Option<u64>,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub render: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ViewContextInput {
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewSourceDescriptor {
    pub path: String,
    pub format: String,
    pub revision: String,
    pub writable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewDocumentDescriptor {
    pub source: ViewSourceDescriptor,
    pub id: String,
    pub name: String,
    pub views: Vec<NamedViewDescriptor>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NamedViewDescriptor {
    pub id: String,
    pub name: String,
    pub properties: Vec<ViewPropertyDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presentation: Option<ViewPresentation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewPropertyDescriptor {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewPresentation {
    #[serde(rename = "type")]
    pub renderer: String,
    pub fallback: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mappings: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, Value>,
}

pub fn stable_named_view_ids(views: &[ObsidianBaseView]) -> Vec<String> {
    let mut seen = BTreeMap::<String, usize>::new();
    views
        .iter()
        .map(|view| {
            let base = identifier(&view.name, "view");
            let occurrence = seen.entry(base.clone()).or_default();
            *occurrence += 1;
            if *occurrence == 1 {
                base
            } else {
                format!("{base}-{}", *occurrence)
            }
        })
        .collect()
}

pub fn identifier(value: &str, fallback: &str) -> String {
    let mut output = String::new();
    let mut previous_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '.') {
            output.push(character.to_ascii_lowercase());
            previous_separator = false;
        } else if !output.is_empty() && !previous_separator {
            output.push('-');
            previous_separator = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output
        .chars()
        .next()
        .is_none_or(|character| !character.is_ascii_alphabetic())
    {
        format!("{fallback}-{output}")
            .trim_end_matches('-')
            .to_string()
    } else {
        output
    }
}

pub fn presentation_for(view: &ObsidianBaseView) -> ViewPresentation {
    ViewPresentation {
        renderer: view.renderer.clone(),
        fallback: "mdbase.table".to_string(),
        mappings: BTreeMap::new(),
        options: view.options.clone(),
    }
}

impl BaseGroupBy {
    pub fn property(&self) -> &str {
        match self {
            Self::Property(property) => property,
            Self::Definition(definition) => &definition.property,
        }
    }

    pub fn direction(&self) -> &str {
        match self {
            Self::Property(_) => "ASC",
            Self::Definition(definition) => &definition.direction,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_open_obsidian_renderer_identifiers() {
        let view = |renderer: &str, group_by: Option<&str>| ObsidianBaseView {
            name: "Dates".to_string(),
            renderer: renderer.to_string(),
            filters: None,
            order: Vec::new(),
            sort: Vec::new(),
            group_by: group_by.map(|property| BaseGroupBy::Property(property.to_string())),
            limit: None,
            options: BTreeMap::new(),
            extensions: BTreeMap::new(),
        };

        let tasknotes = presentation_for(&view("tasknotesKanban", Some("status")));
        assert_eq!(tasknotes.renderer, "tasknotesKanban");
        assert!(tasknotes.mappings.is_empty());

        let unknown = presentation_for(&view("exampleCustomRenderer", None));
        assert_eq!(unknown.renderer, "exampleCustomRenderer");
        assert!(unknown.mappings.is_empty());
    }

    #[test]
    fn deserializes_logical_filter_sequences_as_lists() {
        let document: ObsidianBaseDocument = serde_yaml::from_str(
            r#"views:
  - type: table
    name: Today
    filters:
      and:
        - 'status != "done"'
        - or:
            - 'formula.taskDay.isEmpty()'
            - 'formula.taskDay <= today()'
"#,
        )
        .unwrap();
        let BaseFilter::Logical(filters) = document.views[0].filters.as_ref().unwrap() else {
            panic!("expected a logical view filter");
        };
        let BaseFilterList::Many(and) = filters.and.as_ref().unwrap() else {
            panic!("expected the and sequence to remain a list");
        };
        assert_eq!(and.len(), 2);
        let BaseFilter::Logical(filters) = &and[1] else {
            panic!("expected the nested or filter");
        };
        let BaseFilterList::Many(or) = filters.or.as_ref().unwrap() else {
            panic!("expected the or sequence to remain a list");
        };
        assert_eq!(or.len(), 2);
    }
}
