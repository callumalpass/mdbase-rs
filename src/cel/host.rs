//! The mdbase CEL host (spec Chapter 10): activation, host functions, and
//! value conversion around the standard `cel` engine.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use cel::context::VariableResolver;
use cel::extractors::{Arguments, This};
use cel::objects::{Key, KeyRef, OptionalValue, Value as CelValue};
use cel::{Context, Env, ExecutionError};

/// The standard library is immutable; building it registers every overload, so
/// share one instance instead of rebuilding it for each evaluation.
static STDLIB: LazyLock<Arc<Env>> = LazyLock::new(|| Arc::new(Env::stdlib()));
use chrono::{DateTime, Datelike, NaiveDate, SecondsFormat};
use serde_json::{Map, Value};

use super::program::Program;
use crate::expressions::evaluator::{
    extract_embeds_from_body, extract_links_from_body, extract_tags_from_body, link_value,
    EvalContext, EvaluationClock,
};
use crate::links::linked_files::LinkedFiles;

/// System names a frontmatter field never shadows at the top level.
pub(crate) const RESERVED: [&str; 14] = [
    "record",
    "raw",
    "file",
    "projection",
    "this",
    "values",
    "old",
    "operation",
    "event",
    "workflow",
    "trigger",
    "steps",
    "vars",
    "item",
];

/// Bound on `asFile()` calls in one evaluation; the spec requires at least ten
/// hops of traversal and a bound on total work.
const MAX_LINK_TRAVERSALS: usize = 10_000;

/// Evaluate a compiled program against a host context.
pub(crate) fn evaluate(
    program: &Program,
    context: &EvalContext,
    clock: &EvaluationClock,
) -> Result<Value, String> {
    let needs_facts = program.facts().needs_body_facts;
    let host = Host {
        clock: clock.clone(),
        links: context.all_files.clone(),
        backlinks: context.backlinks_index.clone(),
        source: context.file_path.clone().unwrap_or_default(),
        needs_facts,
        traversals: Arc::new(AtomicUsize::new(0)),
    };
    let date_times = std::cell::OnceCell::new();
    let date_times = || date_times.get_or_init(|| date_time_fields(context));
    let empty = Map::new();
    let bindings = context.frontmatter.as_object().unwrap_or(&empty);
    let record_context = bindings.contains_key("record");
    let used = &program.facts().free_identifiers;

    let mut cel_context = Context::with_env(STDLIB.clone());
    let mut bound = HashSet::new();
    for (name, value) in bindings {
        // Unreferenced fields cannot affect the result; converting them is the
        // dominant per-record cost of wide records.
        if !used.contains(name) {
            continue;
        }
        let converted = match name.as_str() {
            "record" | "raw" | "old" => typed_object(value, date_times()),
            _ if date_times().contains(name) => typed_scalar(value),
            _ => to_cel(value),
        };
        cel_context.add_variable_from_value(name.clone(), converted);
        bound.insert(name.clone());
    }
    if let Some(path) = context.file_path.as_ref().filter(|_| used.contains("file")) {
        let file = host.file_value(
            path,
            bindings,
            context.body.as_deref(),
            &declared_link_selectors(context),
            FileMetadata {
                size: context.file_size,
                mtime: context.file_mtime.as_deref(),
                ctime: context.file_ctime.as_deref(),
            },
        );
        cel_context.add_variable_from_value("file", file);
        bound.insert("file".to_string());
    }
    if bindings.contains_key("projection") && used.contains("this") && !bound.contains("this") {
        let this = context
            .this_context
            .as_deref()
            .map_or(CelValue::Null, |this| host.context_record(this));
        cel_context.add_variable_from_value("this", this);
        bound.insert("this".to_string());
    }
    host.register(&mut cel_context);

    let resolver = MissingFieldsAsNull { bound };
    if record_context {
        cel_context.set_variable_resolver(&resolver);
    }
    let result = super::program::with_stack_for(program.facts().depth, || {
        CelValue::resolve(program.executable(), &cel_context)
    })
    .map_err(|error| error.to_string())?;
    to_json(&result)
}

/// Binds an unreserved identifier that names no bound value, which is a
/// missing record field, to null (spec Chapter 10, "Missing Fields And
/// Presence").
struct MissingFieldsAsNull {
    bound: HashSet<String>,
}

impl VariableResolver for MissingFieldsAsNull {
    fn resolve(&self, variable: &str) -> Option<CelValue> {
        if self.bound.contains(variable) || RESERVED.contains(&variable) {
            return None;
        }
        Some(CelValue::Null)
    }
}

struct FileMetadata<'a> {
    size: Option<u64>,
    mtime: Option<&'a str>,
    ctime: Option<&'a str>,
}

/// Evaluation-scoped data shared with host functions.
#[derive(Clone)]
struct Host {
    clock: EvaluationClock,
    links: Option<Arc<LinkedFiles>>,
    backlinks: Option<Arc<HashMap<String, Vec<String>>>>,
    source: String,
    needs_facts: bool,
    traversals: Arc<AtomicUsize>,
}

impl Host {
    fn register(&self, context: &mut Context) {
        let now = self.clock.instant().fixed_offset();
        context.add_function("now", move || now);
        let today = Arc::new(self.clock.today().to_string());
        context.add_function("today", move || today.clone());
        let host = self.clone();
        context.add_function("date", move |value: CelValue| match value {
            CelValue::String(text) => full_date("date", &text).map(|_| CelValue::String(text)),
            CelValue::Timestamp(instant) => Ok(string(
                &host
                    .clock
                    .date_of(instant.to_utc())
                    .format("%Y-%m-%d")
                    .to_string(),
            )),
            other => Err(ExecutionError::function_error(
                "date",
                format!("expected a string or timestamp, got {}", other.type_of()),
            )),
        });
        let host = self.clone();
        context.add_function("startOfDay", move |This(text): This<Arc<String>>| {
            let date = full_date("startOfDay", &text)?;
            host.clock
                .start_of_day(date)
                .map(|start| CelValue::Timestamp(start.fixed_offset()))
                .ok_or_else(|| {
                    ExecutionError::function_error(
                        "startOfDay",
                        "the day has no representable start",
                    )
                })
        });
        context.add_function("addDays", |This(text): This<Arc<String>>, days: i64| {
            let date = full_date("addDays", &text)?;
            shift(
                date.checked_add_signed(chrono::Duration::days(days)),
                "addDays",
            )
        });
        context.add_function("addMonths", |This(text): This<Arc<String>>, months: i64| {
            let date = full_date("addMonths", &text)?;
            shift(add_months(date, months), "addMonths")
        });
        context.add_function("addYears", |This(text): This<Arc<String>>, years: i64| {
            let date = full_date("addYears", &text)?;
            shift(add_months(date, years.saturating_mul(12)), "addYears")
        });
        context.add_function(
            "daysUntil",
            |This(text): This<Arc<String>>, other: Arc<String>| {
                let from = full_date("daysUntil", &text)?;
                let to = full_date("daysUntil", &other)?;
                Ok::<_, ExecutionError>(CelValue::Int((to - from).num_days()))
            },
        );
        context.add_function("year", |This(text): This<Arc<String>>| {
            full_date("year", &text).map(|date| CelValue::Int(date.year().into()))
        });
        context.add_function("month", |This(text): This<Arc<String>>| {
            full_date("month", &text).map(|date| CelValue::Int(date.month().into()))
        });
        context.add_function("day", |This(text): This<Arc<String>>| {
            full_date("day", &text).map(|date| CelValue::Int(date.day().into()))
        });
        context.add_function("dayOfWeek", |This(text): This<Arc<String>>| {
            full_date("dayOfWeek", &text)
                .map(|date| CelValue::Int(date.weekday().number_from_monday().into()))
        });
        // Unicode default full case mappings, without locale tailoring.
        context.add_function("lower", |This(text): This<Arc<String>>| {
            Ok::<_, ExecutionError>(CelValue::String(Arc::new(text.to_lowercase())))
        });
        context.add_function("upper", |This(text): This<Arc<String>>| {
            Ok::<_, ExecutionError>(CelValue::String(Arc::new(text.to_uppercase())))
        });
        context.add_function(
            "inFolder",
            |This(file): This<CelValue>, folder: Arc<String>| {
                let actual = member_str(&file, "folder").unwrap_or_default();
                let wanted = folder.trim_matches('/');
                actual == wanted || actual.starts_with(&format!("{wanted}/"))
            },
        );
        context.add_function(
            "hasTag",
            |This(file): This<CelValue>, Arguments(tags): Arguments| {
                let present = member_strings(&file, "tags");
                tags.iter().any(|tag| {
                    let CelValue::String(tag) = tag else {
                        return false;
                    };
                    let tag = tag.trim_start_matches('#');
                    present
                        .iter()
                        .any(|existing| existing == tag || existing.starts_with(&format!("{tag}/")))
                })
            },
        );
        let host = self.clone();
        context.add_function(
            "hasLink",
            // A second argument, added by link provenance rewriting, names the
            // record the link was read from.
            move |This(file): This<CelValue>, Arguments(args): Arguments| {
                let (link, link_source) = link_and_source("hasLink", &args, 1)?;
                let source = member_str(&file, "path").unwrap_or_default();
                let wanted = host.resolve_path(&link, link_source.as_deref().unwrap_or(&source));
                Ok::<_, ExecutionError>(member_strings(&file, "links").iter().any(
                    |candidate| match (&wanted, host.resolve_path(candidate, &source)) {
                        (Some(wanted), Some(actual)) => *wanted == actual,
                        _ => link_target(candidate) == link_target(&link),
                    },
                ))
            },
        );
        context.add_function("asLink", |This(file): This<CelValue>| {
            member_str(&file, "path")
                .map(|path| string(&format!("[[{path}]]")))
                .ok_or_else(|| ExecutionError::function_error("asLink", "the value has no path"))
        });
        context.add_function("link", |value: CelValue| match value {
            CelValue::String(text) => Ok(CelValue::String(text)),
            other => Err(ExecutionError::function_error(
                "link",
                format!("expected a string, got {}", other.type_of()),
            )),
        });
        for function in ["asFile", "__mdbase_asFile"] {
            let host = self.clone();
            context.add_function(
                function,
                move |This(link): This<Arc<String>>, Arguments(args): Arguments| {
                    let declaration_source = match (function, args.first()) {
                        ("__mdbase_asFile", Some(CelValue::String(origin))) => origin.as_str(),
                        _ => host.source.as_str(),
                    };
                    let (source, options) = match args.as_slice() {
                        [] => (host.source.as_str(), None),
                        [CelValue::String(source)] => (source.as_str(), None),
                        // Provenance may prepend an inferred source; an explicit
                        // caller source still takes precedence.
                        [CelValue::String(_), CelValue::String(source)]
                            if function == "__mdbase_asFile" =>
                        {
                            (source.as_str(), None)
                        }
                        [options @ CelValue::Map(_)] => (host.source.as_str(), Some(options)),
                        [CelValue::String(source), options @ CelValue::Map(_)] => {
                            (source.as_str(), Some(options))
                        }
                        [CelValue::String(_), CelValue::String(source), options @ CelValue::Map(_)]
                            if function == "__mdbase_asFile" =>
                        {
                            (source.as_str(), Some(options))
                        }
                        _ => {
                            return Err(ExecutionError::function_error(
                                "asFile",
                                "expected an optional source path and an options map",
                            ))
                        }
                    };
                    let options = options.map(link_resolution_options).transpose()?;
                    host.as_file(&link, source, declaration_source, options.as_ref())
                },
            );
        }
    }

    /// The record a link resolves to, in query-candidate shape, or null.
    fn as_file(
        &self,
        link: &str,
        source: &str,
        declaration_source: &str,
        options: Option<&crate::links::resolver::LinkResolutionOptions>,
    ) -> Result<CelValue, ExecutionError> {
        if self.traversals.fetch_add(1, Ordering::Relaxed) >= MAX_LINK_TRAVERSALS {
            return Err(ExecutionError::function_error(
                "asFile",
                format!("link traversal limit of {MAX_LINK_TRAVERSALS} exceeded"),
            ));
        }
        let Some(links) = &self.links else {
            return Ok(CelValue::Null);
        };
        let resolved = links
            .resolve_with_options_from(link, Some(source), Some(declaration_source), options)
            .map_err(|error| {
                ExecutionError::function_error(
                    "asFile",
                    if options.is_some() {
                        format!("{}: {}", error.code, error.message)
                    } else {
                        error.message
                    },
                )
            })?;
        Ok(resolved.map_or(CelValue::Null, |target| {
            let frontmatter = target.frontmatter.as_object().cloned().unwrap_or_default();
            let file = self.file_value(
                &target.path,
                &frontmatter,
                Some(&target.body),
                &[],
                FileMetadata {
                    size: None,
                    mtime: None,
                    ctime: None,
                },
            );
            record_value(&frontmatter, &frontmatter, file)
        }))
    }

    fn resolve_path(&self, link: &str, source: &str) -> Option<String> {
        let links = self.links.as_ref()?;
        links
            .resolve(link, Some(source))
            .ok()
            .flatten()
            .map(|target| target.path.clone())
    }

    /// The `this` record of a query invocation context.
    fn context_record(&self, context: &EvalContext) -> CelValue {
        let bindings = context.frontmatter.as_object().cloned().unwrap_or_default();
        let mut record = bindings
            .iter()
            .map(|(key, value)| (key.clone(), to_cel(value)))
            .collect::<HashMap<_, _>>();
        if let Some(path) = &context.file_path {
            let file = self.file_value(
                path,
                &bindings,
                context.body.as_deref(),
                &declared_link_selectors(context),
                FileMetadata {
                    size: context.file_size,
                    mtime: context.file_mtime.as_deref(),
                    ctime: context.file_ctime.as_deref(),
                },
            );
            record.insert("file".to_string(), file);
        }
        record.into()
    }

    fn file_value(
        &self,
        path: &str,
        frontmatter: &Map<String, Value>,
        body: Option<&str>,
        declared_links: &[String],
        metadata: FileMetadata<'_>,
    ) -> CelValue {
        let location = std::path::Path::new(path);
        let text = |value: Option<&std::ffi::OsStr>| {
            string(value.and_then(|value| value.to_str()).unwrap_or(""))
        };
        let mut file: HashMap<String, CelValue> = HashMap::from([
            ("path".to_string(), string(path)),
            ("name".to_string(), text(location.file_name())),
            ("basename".to_string(), text(location.file_stem())),
            ("ext".to_string(), text(location.extension())),
            (
                "folder".to_string(),
                string(
                    location
                        .parent()
                        .and_then(|value| value.to_str())
                        .unwrap_or(""),
                ),
            ),
        ]);
        if let Some(size) = metadata.size {
            file.insert("size".to_string(), CelValue::Int(size as i64));
        }
        for (key, value) in [("mtime", metadata.mtime), ("ctime", metadata.ctime)] {
            if let Some(value) = value {
                file.insert(
                    key.to_string(),
                    typed_scalar(&Value::String(value.to_string())),
                );
            }
        }
        if let Some(body) = body {
            file.insert("body".to_string(), string(body));
        }
        if self.needs_facts {
            let effective = frontmatter
                .get("record")
                .and_then(Value::as_object)
                .unwrap_or(frontmatter);
            file.insert("tags".to_string(), list(tags(effective, body)));
            file.insert(
                "links".to_string(),
                list(links(effective, body, declared_links)),
            );
            file.insert(
                "embeds".to_string(),
                list(
                    body.map(extract_embeds_from_body)
                        .unwrap_or_default()
                        .iter()
                        .map(|target| link_value(target))
                        .collect::<Vec<_>>(),
                ),
            );
            let mut backlinks = self
                .backlinks
                .as_ref()
                .and_then(|index| index.get(path))
                .cloned()
                .unwrap_or_default();
            backlinks.sort();
            backlinks.dedup();
            file.insert(
                "backlinks".to_string(),
                list(backlinks.into_iter().map(|source| format!("[[{source}]]"))),
            );
        }
        file.into()
    }
}

fn record_value(
    effective: &Map<String, Value>,
    raw: &Map<String, Value>,
    file: CelValue,
) -> CelValue {
    let mut record = effective
        .iter()
        .filter(|(key, _)| !RESERVED.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), to_cel(value)))
        .collect::<HashMap<_, _>>();
    record.insert(
        "record".to_string(),
        to_cel(&Value::Object(effective.clone())),
    );
    record.insert("raw".to_string(), to_cel(&Value::Object(raw.clone())));
    record.insert("file".to_string(), file);
    record.into()
}

/// `file.tags`: frontmatter `tags` followed by inline body tags.
fn tags(frontmatter: &Map<String, Value>, body: Option<&str>) -> Vec<String> {
    let mut tags = match frontmatter.get("tags") {
        Some(Value::String(tag)) if !tag.is_empty() => {
            vec![tag.trim_start_matches('#').to_string()]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(|tag| tag.trim_start_matches('#').to_string())
            .collect(),
        _ => Vec::new(),
    };
    for tag in body.map(extract_tags_from_body).unwrap_or_default() {
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

/// `file.links` in spec order, without de-duplication: declared link fields,
/// other frontmatter wikilinks, then body links.
fn links(frontmatter: &Map<String, Value>, body: Option<&str>, declared: &[String]) -> Vec<String> {
    let object = Value::Object(frontmatter.clone());
    let mut links = Vec::new();
    for selector in declared {
        for value in crate::field_references::get_values(&object, selector) {
            push_link_values(value, &mut links, true);
        }
    }
    let declared_keys = declared
        .iter()
        .filter_map(|selector| crate::field_references::object_path(selector).ok())
        .filter_map(|keys| keys.into_iter().next())
        .collect::<BTreeSet<_>>();
    for (key, value) in frontmatter {
        if !declared_keys.contains(key) {
            push_link_values(value, &mut links, false);
        }
    }
    links.extend(
        body.map(extract_links_from_body)
            .unwrap_or_default()
            .iter()
            .map(|target| link_value(target)),
    );
    links
}

fn push_link_values(value: &Value, links: &mut Vec<String>, declared: bool) {
    match value {
        Value::String(text) if declared || is_wikilink(text) => {
            let mut targets = Vec::new();
            crate::expressions::evaluator::extract_links_from_fm_value(value, &mut targets);
            links.extend(targets.iter().map(|target| link_value(target)));
        }
        Value::Array(items) => {
            for item in items {
                if let Value::String(_) = item {
                    push_link_values(item, links, declared);
                }
            }
        }
        _ => {}
    }
}

fn link_resolution_options(
    value: &CelValue,
) -> Result<crate::links::resolver::LinkResolutionOptions, ExecutionError> {
    let invalid = |message| ExecutionError::function_error("asFile", message);
    let value = to_json(value).map_err(invalid)?;
    let mut options = crate::links::resolver::LinkResolutionOptions::default();
    for (key, value) in value.as_object().expect("options is a CEL map") {
        match key.as_str() {
            "ambiguity" => {
                options.unique = match value.as_str() {
                    Some("native") => false,
                    Some("unique") => true,
                    _ => return Err(invalid("ambiguity must be 'native' or 'unique'".into())),
                }
            }
            "types" => {
                let Some(types) = value.as_array().filter(|types| !types.is_empty()) else {
                    return Err(invalid(
                        "types must be a nonempty list of type names".into(),
                    ));
                };
                for name in types {
                    let Some(name) = name.as_str() else {
                        return Err(invalid("types must contain strings".into()));
                    };
                    crate::types::loader::validate_type_name(name).map_err(invalid)?;
                    options.types.push(name.to_lowercase());
                }
            }
            _ => return Err(invalid(format!("Unknown asFile option '{key}'"))),
        }
    }
    Ok(options)
}

fn is_wikilink(text: &str) -> bool {
    let text = text.trim();
    text.starts_with("[[") && text.ends_with("]]") && !text[2..text.len() - 2].contains("]]")
}

/// The target of a wikilink, Markdown link, or bare path.
fn link_target(text: &str) -> Option<String> {
    let text = text.trim();
    let target = if let Some(inner) = text
        .strip_prefix("[[")
        .and_then(|rest| rest.strip_suffix("]]"))
    {
        inner.split('|').next().unwrap_or(inner)
    } else if let Some((_, rest)) = text
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]("))
    {
        rest.strip_suffix(')').unwrap_or(rest)
    } else {
        text
    };
    let target = target.split('#').next().unwrap_or(target).trim();
    (!target.is_empty()).then(|| target.to_string())
}

/// Link selectors declared in `collection.links` by the record's matched types.
fn declared_link_selectors(context: &EvalContext) -> Vec<String> {
    let (Some(names), Some(types)) = (&context.type_names, &context.types) else {
        return Vec::new();
    };
    let mut selectors = names
        .iter()
        .filter_map(|name| types.get(name))
        .filter_map(|definition| definition.v03_frontmatter.as_ref())
        .filter_map(|frontmatter| frontmatter.pointer("/collection/links"))
        .filter_map(Value::as_object)
        .flat_map(|links| links.keys().cloned())
        .collect::<Vec<_>>();
    selectors.sort();
    selectors.dedup();
    selectors
}

/// Top-level fields that every matched schema declares `format: date-time`.
fn date_time_fields(context: &EvalContext) -> BTreeSet<String> {
    let (Some(names), Some(types)) = (&context.type_names, &context.types) else {
        return BTreeSet::new();
    };
    let schemas = names
        .iter()
        .filter_map(|name| types.get(name))
        .filter_map(|definition| definition.json_schema.as_ref())
        .collect::<Vec<_>>();
    let Some((first, rest)) = schemas.split_first() else {
        return BTreeSet::new();
    };
    let date_time = |schema: &Value, field: &str| {
        schema.pointer(&format!(
            "/properties/{}/format",
            field.replace('~', "~0").replace('/', "~1")
        )) == Some(&Value::String("date-time".to_string()))
    };
    first
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|properties| properties.keys())
        .filter(|field| {
            date_time(first, field) && rest.iter().all(|schema| date_time(schema, field))
        })
        .cloned()
        .collect()
}

fn typed_object(value: &Value, date_times: &BTreeSet<String>) -> CelValue {
    let Some(object) = value.as_object() else {
        return to_cel(value);
    };
    object
        .iter()
        .map(|(key, value)| {
            let converted = if date_times.contains(key) {
                typed_scalar(value)
            } else {
                to_cel(value)
            };
            (key.clone(), converted)
        })
        .collect::<HashMap<_, _>>()
        .into()
}

/// A date-time string becomes a timestamp; a value that fails the format stays
/// as it is and is reported by schema validation.
fn typed_scalar(value: &Value) -> CelValue {
    match value {
        Value::String(text) => DateTime::parse_from_rfc3339(text)
            .map(CelValue::Timestamp)
            .unwrap_or_else(|_| to_cel(value)),
        other => to_cel(other),
    }
}

fn to_cel(value: &Value) -> CelValue {
    match value {
        Value::Null => CelValue::Null,
        Value::Bool(value) => CelValue::Bool(*value),
        Value::Number(number) => number
            .as_i64()
            .map(CelValue::Int)
            .or_else(|| number.as_u64().map(CelValue::UInt))
            .unwrap_or_else(|| CelValue::Float(number.as_f64().unwrap_or(f64::NAN))),
        Value::String(text) => string(text),
        Value::Array(items) => CelValue::List(Arc::new(items.iter().map(to_cel).collect())),
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| (key.clone(), to_cel(value)))
            .collect::<HashMap<_, _>>()
            .into(),
    }
}

/// Serialize a result with the Chapter 10 serialization rules.
fn to_json(value: &CelValue) -> Result<Value, String> {
    Ok(match value {
        CelValue::Null => Value::Null,
        CelValue::Bool(value) => Value::Bool(*value),
        CelValue::Int(value) => Value::from(*value),
        CelValue::UInt(value) => Value::from(*value),
        CelValue::Float(value) => {
            serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number)
        }
        CelValue::String(text) => Value::String(text.to_string()),
        CelValue::List(items) => Value::Array(items.iter().map(to_json).collect::<Result<_, _>>()?),
        CelValue::Map(map) => Value::Object(
            map.map
                .iter()
                .map(|(key, value)| Ok((key_string(key), to_json(value)?)))
                .collect::<Result<_, String>>()?,
        ),
        CelValue::Timestamp(instant) => Value::String(
            instant
                .to_utc()
                .to_rfc3339_opts(SecondsFormat::AutoSi, true),
        ),
        CelValue::Duration(duration) => Value::String(duration_string(duration)),
        CelValue::Opaque(_) => {
            let optional = <&OptionalValue>::try_from(value)
                .map_err(|_| "an opaque value cannot be serialized".to_string())?;
            optional.value().map_or(Ok(Value::Null), to_json)?
        }
        other => return Err(format!("a {} value cannot be serialized", other.type_of())),
    })
}

fn key_string(key: &Key) -> String {
    match key {
        Key::Int(value) => value.to_string(),
        Key::Uint(value) => value.to_string(),
        Key::Bool(value) => value.to_string(),
        Key::String(value) => value.to_string(),
    }
}

/// A CEL duration string in seconds, such as `"5400s"` or `"1.5s"`.
fn duration_string(duration: &chrono::Duration) -> String {
    let nanos = duration.num_nanoseconds().unwrap_or(i64::MAX);
    let seconds = nanos / 1_000_000_000;
    let fraction = (nanos % 1_000_000_000).abs();
    if fraction == 0 {
        format!("{seconds}s")
    } else {
        let digits = format!("{fraction:09}");
        format!("{seconds}.{}s", digits.trim_end_matches('0'))
    }
}

fn member<'a>(value: &'a CelValue, key: &str) -> Option<&'a CelValue> {
    match value {
        CelValue::Map(map) => map.get(&KeyRef::String(key)),
        _ => None,
    }
}

fn member_str(value: &CelValue, key: &str) -> Option<String> {
    match member(value, key)? {
        CelValue::String(text) => Some(text.to_string()),
        _ => None,
    }
}

fn member_strings(value: &CelValue, key: &str) -> Vec<String> {
    match member(value, key) {
        Some(CelValue::List(items)) => items
            .iter()
            .filter_map(|item| match item {
                CelValue::String(text) => Some(text.to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn string(text: &str) -> CelValue {
    CelValue::String(Arc::new(text.to_string()))
}

fn list(items: impl IntoIterator<Item = String>) -> CelValue {
    CelValue::List(Arc::new(
        items.into_iter().map(|item| string(&item)).collect(),
    ))
}

fn full_date(function: &str, text: &str) -> Result<NaiveDate, ExecutionError> {
    if text.len() != 10 {
        return Err(not_a_date(function, text));
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| not_a_date(function, text))
}

fn not_a_date(function: &str, text: &str) -> ExecutionError {
    ExecutionError::function_error(function, format!("'{text}' is not an RFC 3339 full-date"))
}

fn shift(date: Option<NaiveDate>, function: &str) -> Result<CelValue, ExecutionError> {
    date.map(|date| string(&date.format("%Y-%m-%d").to_string()))
        .ok_or_else(|| ExecutionError::function_error(function, "the date is out of range"))
}

/// Calendar month arithmetic that clamps to the last day of the target month.
fn add_months(date: NaiveDate, months: i64) -> Option<NaiveDate> {
    let index = i64::from(date.year()) * 12 + i64::from(date.month0()) + months;
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = u32::try_from(index.rem_euclid(12)).ok()? + 1;
    (1..=date.day())
        .rev()
        .find_map(|day| NaiveDate::from_ymd_opt(year, month, day))
}

/// The link argument of a link helper and the optional source path that link
/// provenance rewriting appends after `arity` arguments.
fn link_and_source(
    function: &str,
    args: &[CelValue],
    arity: usize,
) -> Result<(Arc<String>, Option<Arc<String>>), ExecutionError> {
    let text = |value: &CelValue| match value {
        CelValue::String(text) => Ok(text.clone()),
        other => Err(ExecutionError::function_error(
            function,
            format!("expected a string, got {}", other.type_of()),
        )),
    };
    match args {
        [link] if arity == 1 => Ok((text(link)?, None)),
        [link, source] if arity == 1 => Ok((text(link)?, Some(text(source)?))),
        _ => Err(ExecutionError::function_error(
            function,
            "expected a link and an optional source path",
        )),
    }
}
