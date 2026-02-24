//! Query result envelope.

use crate::expressions::evaluator::{evaluate as eval_expr, EvalContext};
use crate::expressions::parser::Parser as ExprParser;
use crate::Collection;
use std::collections::HashMap;

impl Collection {
    /// Compute property summaries over a set of candidates.
    pub(crate) fn compute_summaries(
        &self,
        candidates: &[serde_json::Value],
        property_summaries: &HashMap<String, String>,
        custom_summaries: &HashMap<String, String>,
    ) -> serde_json::Value {
        let mut summaries = serde_json::Map::new();

        for (field, summary_type) in property_summaries {
            // Collect values for this field from all candidates
            let values: Vec<&serde_json::Value> = candidates
                .iter()
                .filter_map(|c| c.get("frontmatter").and_then(|fm| fm.get(field)))
                .collect();

            let result = if let Some(formula) = custom_summaries.get(summary_type) {
                // Custom summary: evaluate formula with `values` array in context
                self.evaluate_custom_summary(formula, field, candidates)
            } else {
                match summary_type.as_str() {
                    "Average" => {
                        let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
                        if nums.is_empty() {
                            serde_json::Value::Null
                        } else {
                            let sum: f64 = nums.iter().sum();
                            let avg = sum / nums.len() as f64;
                            // Return integer if it's a whole number
                            if avg == avg.floor() && avg.abs() < i64::MAX as f64 {
                                serde_json::json!(avg as i64)
                            } else {
                                serde_json::json!(avg)
                            }
                        }
                    }
                    "Sum" => {
                        let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
                        let has_float = values
                            .iter()
                            .any(|v| v.is_f64() && !v.is_i64() && !v.is_u64());
                        if nums.is_empty() {
                            serde_json::json!(0)
                        } else {
                            let sum: f64 = nums.iter().sum();
                            if !has_float && sum == sum.floor() && sum.abs() < i64::MAX as f64 {
                                serde_json::json!(sum as i64)
                            } else {
                                serde_json::json!(sum)
                            }
                        }
                    }
                    "Min" => {
                        let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
                        if nums.is_empty() {
                            serde_json::Value::Null
                        } else {
                            let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
                            if min == min.floor() && min.abs() < i64::MAX as f64 {
                                serde_json::json!(min as i64)
                            } else {
                                serde_json::json!(min)
                            }
                        }
                    }
                    "Max" => {
                        let nums: Vec<f64> = values.iter().filter_map(|v| v.as_f64()).collect();
                        if nums.is_empty() {
                            serde_json::Value::Null
                        } else {
                            let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                            if max == max.floor() && max.abs() < i64::MAX as f64 {
                                serde_json::json!(max as i64)
                            } else {
                                serde_json::json!(max)
                            }
                        }
                    }
                    "Earliest" => {
                        let strs: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
                        strs.iter()
                            .min()
                            .map(|s| serde_json::json!(s))
                            .unwrap_or(serde_json::Value::Null)
                    }
                    "Latest" => {
                        let strs: Vec<&str> = values.iter().filter_map(|v| v.as_str()).collect();
                        strs.iter()
                            .max()
                            .map(|s| serde_json::json!(s))
                            .unwrap_or(serde_json::Value::Null)
                    }
                    "Checked" => {
                        let count = values.iter().filter(|v| v.as_bool() == Some(true)).count();
                        serde_json::json!(count)
                    }
                    "Unchecked" => {
                        let count = values.iter().filter(|v| v.as_bool() == Some(false)).count();
                        serde_json::json!(count)
                    }
                    "Empty" => {
                        let count = candidates
                            .iter()
                            .filter(|c| {
                                let val = c.get("frontmatter").and_then(|fm| fm.get(field));
                                val.is_none()
                                    || val == Some(&serde_json::Value::Null)
                                    || val.and_then(|v| v.as_str()) == Some("")
                            })
                            .count();
                        serde_json::json!(count)
                    }
                    "Filled" => {
                        let count = candidates
                            .iter()
                            .filter(|c| {
                                let val = c.get("frontmatter").and_then(|fm| fm.get(field));
                                val.is_some()
                                    && val != Some(&serde_json::Value::Null)
                                    && val.and_then(|v| v.as_str()) != Some("")
                            })
                            .count();
                        serde_json::json!(count)
                    }
                    "Unique" => {
                        let mut unique_vals: Vec<serde_json::Value> = Vec::new();
                        for v in &values {
                            if !unique_vals.contains(v) {
                                unique_vals.push((*v).clone());
                            }
                        }
                        serde_json::json!(unique_vals.len())
                    }
                    _ => serde_json::Value::Null,
                }
            };

            summaries.insert(field.clone(), result);
        }

        serde_json::Value::Object(summaries)
    }

    /// Evaluate a custom summary formula with `values` array in context.
    pub(crate) fn evaluate_custom_summary(
        &self,
        formula: &str,
        field: &str,
        candidates: &[serde_json::Value],
    ) -> serde_json::Value {
        // Collect numeric values for the field
        let values: Vec<serde_json::Value> = candidates
            .iter()
            .filter_map(|c| c.get("frontmatter").and_then(|fm| fm.get(field)).cloned())
            .collect();

        let values_array = serde_json::Value::Array(values);

        // Build context with `values` as a top-level field
        let ctx = EvalContext {
            frontmatter: serde_json::json!({"values": values_array}),
            raw_frontmatter: None,
            file_path: None,
            body: None,
            file_size: None,
            file_mtime: None,
            file_ctime: None,
            this_context: None,
            all_files: None,
            traversal_depth: std::cell::Cell::new(0),
            backlinks_index: None,
            type_names: None,
            types: None,
            string_concat: true,
        };

        match ExprParser::parse(formula) {
            Ok(parsed) => match eval_expr(&parsed, &ctx) {
                Ok(val) => val,
                Err(_) => serde_json::Value::Null,
            },
            Err(_) => serde_json::Value::Null,
        }
    }
}
