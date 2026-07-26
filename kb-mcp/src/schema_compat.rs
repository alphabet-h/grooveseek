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
//! explicit `null` into `None`.
//!
//! Dropping a width `format` is not free, though: it is the only place the upper
//! bound of a Rust integer type is stated, since `schemars` emits `minimum: 0` for
//! unsigned types but never a `maximum`. So the range is written out as explicit
//! `minimum` / `maximum` before the keyword is removed — otherwise the advertised
//! schema would be *wider* than what the server accepts.
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
            normalize_format(obj);
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
///
/// For integer widths the range the format implied is written out explicitly
/// first. `schemars` emits `minimum: 0` for unsigned types but never a
/// `maximum`, so dropping the keyword on its own would advertise a *wider*
/// domain than the server accepts: a client would be told `4294967296` is a
/// valid `u32`, and serde would then reject it during deserialisation, before
/// any handler could clamp it. Bounds already present win — an explicit
/// constraint from the type is more specific than the width default.
fn normalize_format(obj: &mut Map<String, Value>) {
    let format = match obj.get("format") {
        Some(Value::String(format)) if NONSTANDARD_FORMATS.contains(&format.as_str()) => {
            format.clone()
        }
        _ => return,
    };
    obj.remove("format");

    if let Some((minimum, maximum)) = integer_bounds(&format) {
        obj.entry("minimum").or_insert(minimum);
        obj.entry("maximum").or_insert(maximum);
    }
}

/// The inclusive range a Rust integer-width `format` stands for.
///
/// `float` / `double` deliberately return `None`. Their Rust ranges (±3.4e38 for
/// `f32`) are not a meaningful thing to advertise to a caller, and every float
/// parameter kb-mcp exposes already carries its real, much narrower range in the
/// description and in server-side validation.
///
/// `uint` is what `schemars` emits for `usize`, whose width is platform
/// dependent; the 64-bit bound is used, which is the permissive choice on a
/// 32-bit target and therefore no worse than advertising no bound at all.
fn integer_bounds(format: &str) -> Option<(Value, Value)> {
    let bounds = match format {
        "uint8" => (Value::from(0), Value::from(u8::MAX)),
        "uint16" => (Value::from(0), Value::from(u16::MAX)),
        "uint32" => (Value::from(0), Value::from(u32::MAX)),
        "uint" | "uint64" => (Value::from(0), Value::from(u64::MAX)),
        "int8" => (Value::from(i8::MIN), Value::from(i8::MAX)),
        "int16" => (Value::from(i16::MIN), Value::from(i16::MAX)),
        "int32" => (Value::from(i32::MIN), Value::from(i32::MAX)),
        "int64" => (Value::from(i64::MIN), Value::from(i64::MAX)),
        _ => return None,
    };
    Some(bounds)
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
        // The width format is the only statement of the upper bound, so it has
        // to become an explicit `maximum` rather than just disappearing.
        let out = transformed(serde_json::json!({
            "type": ["integer", "null"],
            "format": "uint32",
            "minimum": 0
        }));
        assert_eq!(
            out,
            serde_json::json!({"type": "integer", "minimum": 0, "maximum": 4294967295u32})
        );
    }

    #[test]
    fn test_signed_width_format_becomes_both_bounds() {
        let out = transformed(serde_json::json!({"type": "integer", "format": "int8"}));
        assert_eq!(
            out,
            serde_json::json!({"type": "integer", "minimum": -128, "maximum": 127})
        );
    }

    #[test]
    fn test_existing_bounds_win_over_width_defaults() {
        // A bound that came from the type itself is more specific than the
        // width default and must not be widened.
        let out = transformed(serde_json::json!({
            "type": "integer",
            "format": "uint32",
            "minimum": 1,
            "maximum": 100
        }));
        assert_eq!(
            out,
            serde_json::json!({"type": "integer", "minimum": 1, "maximum": 100})
        );
    }

    #[test]
    fn test_float_format_is_dropped_without_inventing_bounds() {
        // f32's ±3.4e38 range is not useful to advertise; the real range lives
        // in the description and in server-side validation.
        let out = transformed(serde_json::json!({"type": ["number", "null"], "format": "float"}));
        assert_eq!(out, serde_json::json!({"type": "number"}));
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
                    "limit": {"type": "integer", "minimum": 0, "maximum": 4294967295u32},
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
