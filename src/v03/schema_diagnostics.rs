use jsonschema::error::{ValidationError, ValidationErrorKind};

use super::Diagnostic;

pub(super) fn validation_diagnostic(
    error: ValidationError<'_>,
    path: &str,
    schema_id: &str,
    type_name: Option<&str>,
) -> Diagnostic {
    Diagnostic {
        severity: "error".to_string(),
        code: diagnostic_code(&error).to_string(),
        message: error.to_string(),
        path: Some(path.to_string()),
        field: diagnostic_field(&error),
        type_name: type_name.map(str::to_string),
        schema_location: Some(format!("{schema_id}#{}", error.schema_path)),
        details: Some(serde_json::json!({
            "instance_path": error.instance_path.to_string(),
            "schema_path": error.schema_path.to_string(),
        })),
    }
}

fn diagnostic_code(error: &ValidationError<'_>) -> &'static str {
    match &error.kind {
        ValidationErrorKind::Required { .. } => "schema_required",
        ValidationErrorKind::AdditionalProperties { .. } => "schema_additional_properties",
        ValidationErrorKind::UnevaluatedProperties { .. } => "schema_unevaluated_properties",
        ValidationErrorKind::Type { .. } => "schema_type",
        ValidationErrorKind::Constant { .. } => "schema_const",
        ValidationErrorKind::Enum { .. } => "schema_enum",
        ValidationErrorKind::Pattern { .. } => "schema_pattern",
        ValidationErrorKind::PropertyNames { error } => diagnostic_code(error),
        ValidationErrorKind::MinLength { .. } => "schema_min_length",
        ValidationErrorKind::MaxLength { .. } => "schema_max_length",
        ValidationErrorKind::Minimum { .. } => "schema_minimum",
        ValidationErrorKind::Maximum { .. } => "schema_maximum",
        ValidationErrorKind::MultipleOf { .. } => "schema_multiple_of",
        ValidationErrorKind::ExclusiveMinimum { .. } => "schema_exclusive_minimum",
        ValidationErrorKind::ExclusiveMaximum { .. } => "schema_exclusive_maximum",
        ValidationErrorKind::MinItems { .. } => "schema_min_items",
        ValidationErrorKind::MaxItems { .. } => "schema_max_items",
        ValidationErrorKind::UniqueItems => "schema_unique_items",
        ValidationErrorKind::OneOfNotValid
            if error.instance.as_str() == Some("")
                && error.schema_path.to_string() == "/properties/select/items/oneOf" =>
        {
            "schema_min_length"
        }
        ValidationErrorKind::OneOfMultipleValid | ValidationErrorKind::OneOfNotValid => {
            "schema_one_of"
        }
        ValidationErrorKind::AnyOf => "schema_any_of",
        ValidationErrorKind::Not { .. } => "schema_not",
        ValidationErrorKind::Format { .. } => "format_invalid",
        _ => "schema_invalid",
    }
}

fn diagnostic_field(error: &ValidationError<'_>) -> Option<String> {
    match &error.kind {
        ValidationErrorKind::Required { property } => property.as_str().map(str::to_string),
        ValidationErrorKind::AdditionalProperties { unexpected }
        | ValidationErrorKind::UnevaluatedProperties { unexpected } => unexpected.first().cloned(),
        _ => error
            .instance_path
            .to_string()
            .rsplit('/')
            .find(|segment| !segment.is_empty())
            .map(decode_pointer_segment),
    }
}

fn decode_pointer_segment(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}
