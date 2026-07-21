use serde_json::Value;

use super::model::RuntimeDiagnostic;

pub(crate) fn contract_markdown(
    contract: &Value,
    body: Option<&str>,
) -> Result<String, Box<RuntimeDiagnostic>> {
    let id = contract.get("id").and_then(Value::as_str).ok_or_else(|| {
        Box::new(RuntimeDiagnostic::error(
            "invalid_contract",
            "A materialized contract requires a string id.",
        ))
    })?;
    let frontmatter = serde_yaml::to_string(contract).map_err(|error| {
        Box::new(
            RuntimeDiagnostic::error(
                "materialization_failed",
                format!("Contract {id} could not be encoded as YAML: {error}"),
            )
            .for_id(id),
        )
    })?;
    let heading = contract.get("name").and_then(Value::as_str).unwrap_or(id);
    let generated_body;
    let body = match body {
        Some(body) => body,
        None => {
            generated_body = format!(
                "# {heading}\n\nMaterialized runtime contract for `{}`.\n",
                id.replace('`', "\\`")
            );
            &generated_body
        }
    };
    Ok(format!(
        "---\n{}\n---\n\n{}",
        frontmatter.trim_start_matches("---\n").trim_end(),
        body.trim_start_matches('\n')
    ))
}
