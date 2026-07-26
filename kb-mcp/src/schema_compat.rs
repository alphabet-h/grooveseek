//! Normalisation applied to the JSON Schema kb-mcp advertises for its MCP tools.
//!
//! `schemars` derives `Option<T>` as a union type — `{"type": ["string", "null"]}` —
//! and annotates Rust integer/float widths with `format` values such as `uint32`.
//! Both are valid JSON Schema 2020-12, and MCP clients built on the official SDKs
//! handle them, but they are a well-known interoperability hazard elsewhere:
//!
//! - OpenAI-style function calling rejects `null` inside a union type.
//! - Runtimes that compile the schema into a decoding grammar (llama.cpp, Ollama,
//!   vLLM) have long-standing bugs converting union types; the usual workaround
//!   published for them is exactly "strip `null` out of the type array".
//! - `uint32` / `float` are not part of the JSON Schema format vocabulary, so a
//!   strict validator may reject them.
//!
//! When a runtime fails to build a call, the model tends to fall back to emitting
//! its raw tool-call template as plain text, which never reaches the server at all
//! — the symptom reported in issue #75.
//!
//! Dropping `null` does not narrow what the server accepts. Optionality is already
//! carried by the field's absence from `required`, and serde still deserialises an
//! explicit `null` into `None`. Dropping the width `format` loses nothing either:
//! the actual bound travels in `minimum` / `maximum`, which are preserved.
//!
//! The MCP specification's own examples use plain single types, so this brings the
//! advertised schema closer to the shape clients are most likely to expect.
//!
//! The `$schema` key `rmcp` writes into `inputSchema` is deliberately left alone:
//! `rmcp` sets the 2020-12 meta-schema on purpose to match the specification, and
//! it is added after this transform runs anyway.

// `schemars` reaches us through `rmcp`'s re-export rather than as a direct
// dependency, so that both sides always agree on the same crate version.
use rmcp::schemars;
use schemars::Schema;
use schemars::transform::{Transform, transform_subschemas};
use serde_json::{Map, Value};

/// `format` values emitted by `schemars` for Rust numeric widths. None of these
/// are in the JSON Schema format vocabulary.
pub(crate) const NONSTANDARD_FORMATS: &[&str] = &[
    "uint", "uint8", "uint16", "uint32", "uint64", "int8", "int16", "int32", "int64", "float",
    "double",
];

/// Rewrites a generated schema into the conservative subset described in the
/// module docs. Applied via `#[schemars(transform = ...)]` on every tool
/// parameter struct, and recursively to all subschemas.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientCompat;

impl Transform for ClientCompat {
    fn transform(&mut self, schema: &mut Schema) {
        if let Some(obj) = schema.as_object_mut() {
            collapse_nullable_type(obj);
            drop_nonstandard_format(obj);
        }
        transform_subschemas(self, schema);
    }
}

/// `{"type": ["string", "null"]}` becomes `{"type": "string"}`.
///
/// A type array that does not mention `null` is left alone, and so is one that
/// mentions *only* `null` — removing it there would leave an empty type array,
/// which is a stricter (and different) statement than the original.
fn collapse_nullable_type(obj: &mut Map<String, Value>) {
    let types = match obj.get("type") {
        Some(Value::Array(types)) => types.clone(),
        _ => return,
    };

    let kept: Vec<Value> = types
        .iter()
        .filter(|t| t.as_str() != Some("null"))
        .cloned()
        .collect();

    if kept.len() == types.len() || kept.is_empty() {
        return;
    }

    let replacement = if kept.len() == 1 {
        kept.into_iter().next().expect("length checked above")
    } else {
        Value::Array(kept)
    };
    obj.insert("type".to_string(), replacement);
}

/// Removes Rust-width `format` annotations, keeping standard ones (`date-time`,
/// `uri`, ...) untouched.
fn drop_nonstandard_format(obj: &mut Map<String, Value>) {
    let is_nonstandard = matches!(
        obj.get("format"),
        Some(Value::String(format)) if NONSTANDARD_FORMATS.contains(&format.as_str())
    );
    if is_nonstandard {
        obj.remove("format");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transformed(value: Value) -> Value {
        let mut schema = Schema::try_from(value).expect("valid schema value");
        ClientCompat.transform(&mut schema);
        schema.to_value()
    }

    #[test]
    fn test_nullable_union_collapses_to_single_type() {
        let out = transformed(serde_json::json!({"type": ["string", "null"]}));
        assert_eq!(out, serde_json::json!({"type": "string"}));
    }

    #[test]
    fn test_union_without_null_is_untouched() {
        let out = transformed(serde_json::json!({"type": ["string", "integer"]}));
        assert_eq!(out, serde_json::json!({"type": ["string", "integer"]}));
    }

    #[test]
    fn test_multi_type_union_keeps_array_after_dropping_null() {
        let out = transformed(serde_json::json!({"type": ["string", "integer", "null"]}));
        assert_eq!(out, serde_json::json!({"type": ["string", "integer"]}));
    }

    #[test]
    fn test_null_only_type_is_left_alone() {
        // Removing "null" here would leave an empty type array, which says
        // something different from the original schema.
        let out = transformed(serde_json::json!({"type": ["null"]}));
        assert_eq!(out, serde_json::json!({"type": ["null"]}));
    }

    #[test]
    fn test_scalar_type_is_left_alone() {
        let out = transformed(serde_json::json!({"type": "string"}));
        assert_eq!(out, serde_json::json!({"type": "string"}));
    }

    #[test]
    fn test_nonstandard_format_is_dropped_but_bounds_survive() {
        let out = transformed(serde_json::json!({
            "type": ["integer", "null"],
            "format": "uint32",
            "minimum": 0
        }));
        assert_eq!(out, serde_json::json!({"type": "integer", "minimum": 0}));
    }

    #[test]
    fn test_standard_format_is_preserved() {
        let out = transformed(serde_json::json!({"type": "string", "format": "date-time"}));
        assert_eq!(
            out,
            serde_json::json!({"type": "string", "format": "date-time"})
        );
    }

    #[test]
    fn test_transform_reaches_nested_properties() {
        let out = transformed(serde_json::json!({
            "type": "object",
            "properties": {
                "limit": {"type": ["integer", "null"], "format": "uint32"},
                "tags": {
                    "type": ["array", "null"],
                    "items": {"type": ["string", "null"]}
                }
            }
        }));
        assert_eq!(
            out,
            serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer"},
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"}
                    }
                }
            })
        );
    }

    #[test]
    fn test_transform_reaches_combinator_branches() {
        let out = transformed(serde_json::json!({
            "anyOf": [
                {"type": ["string", "null"]},
                {"type": ["number", "null"], "format": "float"}
            ]
        }));
        assert_eq!(
            out,
            serde_json::json!({
                "anyOf": [
                    {"type": "string"},
                    {"type": "number"}
                ]
            })
        );
    }
}
