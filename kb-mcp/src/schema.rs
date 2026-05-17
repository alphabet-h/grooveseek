//! YAML frontmatter の構造規約を定義する TOML スキーマと
//! それに対するバリデータ。
//!
//! スキーマ記述例 (`kb-mcp-schema.toml`):
//!
//! ```toml
//! [fields.title]
//! required = true
//! type = "string"
//! min_length = 1
//!
//! [fields.date]
//! required = true
//! type = "string"
//! pattern = '^\d{4}-\d{2}-\d{2}$'
//!
//! [fields.topic]
//! required = true
//! type = "string"
//! enum = ["mcp", "rag", "ai"]
//!
//! [fields.tags]
//! required = true
//! type = "array"
//! min_length = 1
//! ```
//!
//! `validate(fm, schema)` は `Frontmatter` 構造体に対して違反を返す。
//! CLI `kb-mcp validate` サブコマンドから呼ばれる。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::parser::Frontmatter;

// ---------------------------------------------------------------------------
// Schema types
// ---------------------------------------------------------------------------

/// Known frontmatter fields. Schema の keys はこのうちどれかでなければ
/// unsupported field としてエラー (loader 段階で弾く)。
const KNOWN_FIELDS: &[&str] = &["title", "date", "topic", "depth", "tags"];

/// `kb-mcp-schema.toml` のルート構造。
///
/// MVP では `fields` のみ。`[options]` セクションは future-only として
/// planner 仕様書で予告しているが現行実装では持たない (dead flag を避けるため)。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawSchema {
    #[serde(default)]
    pub fields: BTreeMap<String, FieldRule>,
}

/// 個々のフィールドに対する検証ルール。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    /// `true` なら欠落 (None) 時に MissingRequired 違反。
    #[serde(default)]
    pub required: bool,
    /// `"string"` / `"array"` / `"date"` / `"integer"`。
    /// 現状 Frontmatter は string と array<string> のみだが、スキーマ側は
    /// ユーザの書き方を広めに受け入れ、loader が妥当性を判断する。
    #[serde(default, rename = "type")]
    pub field_type: Option<FieldType>,
    /// 正規表現 (Rust `regex` 互換)。string 型にのみ適用。
    #[serde(default)]
    pub pattern: Option<String>,
    /// 許容値リスト。string / array 要素に対して完全一致をチェック。
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    /// string なら文字数、array なら要素数の下限 (inclusive)。
    #[serde(default)]
    pub min_length: Option<usize>,
    /// string なら文字数、array なら要素数の上限 (inclusive)。
    #[serde(default)]
    pub max_length: Option<usize>,
    /// `required = true` のとき、空文字列 / 空配列を許容するか。
    /// 既定 false (空も違反扱い)。
    #[serde(default)]
    pub allow_empty: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    String,
    Integer,
    Date,
    Array,
}

impl FieldType {
    fn as_str(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Integer => "integer",
            FieldType::Date => "date",
            FieldType::Array => "array",
        }
    }
}

// ---------------------------------------------------------------------------
// Compiled schema (runtime 表現)
// ---------------------------------------------------------------------------

/// `RawSchema` を load 時に検証 + コンパイルしたもの。
/// Validate 側のホットパスで string → Regex の再コンパイルを避けるために分離。
#[derive(Debug)]
pub struct Schema {
    pub fields: BTreeMap<String, CompiledRule>,
}

#[derive(Debug)]
pub struct CompiledRule {
    pub required: bool,
    pub field_type: Option<FieldType>,
    pub pattern: Option<Regex>,
    pub enum_values: Option<Vec<String>>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub allow_empty: bool,
}

impl Schema {
    /// TOML 文字列からスキーマを読み、コンパイルして返す。
    /// 未サポートキー (KNOWN_FIELDS 以外) や不正な regex はここで reject する。
    pub fn from_toml_str(src: &str) -> Result<Self> {
        let raw: RawSchema = toml::from_str(src).context("failed to parse schema TOML")?;
        Self::compile(raw)
    }

    /// ファイルパスから読み込み。存在しなければ `None` を返す。
    pub fn load_optional(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read schema: {}", path.display()))?;
        let schema = Self::from_toml_str(&text)
            .with_context(|| format!("failed to compile schema: {}", path.display()))?;
        Ok(Some(schema))
    }

    fn compile(raw: RawSchema) -> Result<Self> {
        let mut out: BTreeMap<String, CompiledRule> = BTreeMap::new();
        for (name, rule) in raw.fields {
            if !KNOWN_FIELDS.contains(&name.as_str()) {
                anyhow::bail!(
                    "unsupported field {:?} in schema. Known fields: {:?}",
                    name,
                    KNOWN_FIELDS
                );
            }
            // array フィールドに pattern が付いていても現状意味がないため
            // 明示的にはじく (将来 array 要素への pattern を入れる場合は要拡張)
            if rule.field_type == Some(FieldType::Array) && rule.pattern.is_some() {
                anyhow::bail!("field {name:?}: `pattern` is only valid for string-typed fields");
            }
            // MVP では Frontmatter 側が全フィールドを string で持つため、
            // integer / date は実装されていない。silent pass を避けるため
            // compile 段階で reject し、代わりに pattern で表現するよう誘導する。
            if matches!(
                rule.field_type,
                Some(FieldType::Integer) | Some(FieldType::Date)
            ) {
                anyhow::bail!(
                    "field {name:?}: type = {:?} is not implemented in MVP. \
                     Use `type = \"string\"` with `pattern = '...'` for now \
                     (e.g. ISO date: `pattern = '^\\d{{4}}-\\d{{2}}-\\d{{2}}$'`).",
                    rule.field_type.unwrap().as_str()
                );
            }
            let pattern = match &rule.pattern {
                Some(p) => Some(
                    Regex::new(p).with_context(|| format!("invalid regex for field {name:?}"))?,
                ),
                None => None,
            };
            out.insert(
                name,
                CompiledRule {
                    required: rule.required,
                    field_type: rule.field_type,
                    pattern,
                    enum_values: rule.enum_values,
                    min_length: rule.min_length,
                    max_length: rule.max_length,
                    allow_empty: rule.allow_empty,
                },
            );
        }
        Ok(Schema { fields: out })
    }
}

// ---------------------------------------------------------------------------
// Violations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Violation {
    MissingRequired {
        field: String,
    },
    TypeMismatch {
        field: String,
        expected: String,
        actual: String,
    },
    PatternMismatch {
        field: String,
        pattern: String,
        actual: String,
    },
    NotInEnum {
        field: String,
        actual: String,
        allowed: Vec<String>,
    },
    LengthOutOfRange {
        field: String,
        actual: usize,
        min: Option<usize>,
        max: Option<usize>,
    },
}

impl Violation {
    pub fn field(&self) -> &str {
        match self {
            Violation::MissingRequired { field } => field,
            Violation::TypeMismatch { field, .. } => field,
            Violation::PatternMismatch { field, .. } => field,
            Violation::NotInEnum { field, .. } => field,
            Violation::LengthOutOfRange { field, .. } => field,
        }
    }

    /// 人間向けの 1 行メッセージ。
    pub fn message(&self) -> String {
        match self {
            Violation::MissingRequired { field } => {
                format!("{field} is required but missing (or empty)")
            }
            Violation::TypeMismatch {
                field,
                expected,
                actual,
            } => {
                format!("{field} expected {expected} but got {actual}")
            }
            Violation::PatternMismatch {
                field,
                pattern,
                actual,
            } => {
                format!("{field} {actual:?} does not match pattern {pattern}")
            }
            Violation::NotInEnum {
                field,
                actual,
                allowed,
            } => {
                format!("{field} {actual:?} is not in enum [{}]", allowed.join(", "))
            }
            Violation::LengthOutOfRange {
                field,
                actual,
                min,
                max,
            } => {
                let range = match (min, max) {
                    (Some(lo), Some(hi)) => format!("{lo}..={hi}"),
                    (Some(lo), None) => format!(">= {lo}"),
                    (None, Some(hi)) => format!("<= {hi}"),
                    (None, None) => "unknown".to_string(),
                };
                format!("{field} length {actual} is out of range {range}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// `Frontmatter` を `Schema` に照らして違反リストを返す。空リストなら OK。
pub fn validate(fm: &Frontmatter, schema: &Schema) -> Vec<Violation> {
    let mut out = Vec::new();

    for (name, rule) in &schema.fields {
        match name.as_str() {
            "title" => check_string(&mut out, name, rule, fm.title.as_deref()),
            "date" => check_string(&mut out, name, rule, fm.date.as_deref()),
            "topic" => check_string(&mut out, name, rule, fm.topic.as_deref()),
            "depth" => check_string(&mut out, name, rule, fm.depth.as_deref()),
            "tags" => check_tags(&mut out, name, rule, &fm.tags),
            _ => {} // schema::compile で弾いているので到達しない
        }
    }

    out
}

fn check_string(out: &mut Vec<Violation>, name: &str, rule: &CompiledRule, value: Option<&str>) {
    // MVP では Frontmatter 側が全フィールド string なので、許容する型は
    // `string` のみ。`integer` / `date` は compile 段階で reject 済 (後ろに
    // 到達しない)。array は明示的に mismatch 扱い。
    if let Some(ft) = rule.field_type
        && ft != FieldType::String
    {
        out.push(Violation::TypeMismatch {
            field: name.to_string(),
            expected: ft.as_str().to_string(),
            actual: "string".to_string(),
        });
        return;
    }

    let Some(v) = value else {
        if rule.required {
            out.push(Violation::MissingRequired {
                field: name.to_string(),
            });
        }
        return;
    };

    // 空文字列の扱い: required && !allow_empty なら空も Missing 扱い
    if v.is_empty() && rule.required && !rule.allow_empty {
        out.push(Violation::MissingRequired {
            field: name.to_string(),
        });
        return;
    }

    // length
    let len = v.chars().count();
    if let Some(min) = rule.min_length
        && len < min
    {
        out.push(Violation::LengthOutOfRange {
            field: name.to_string(),
            actual: len,
            min: Some(min),
            max: rule.max_length,
        });
    }
    if let Some(max) = rule.max_length
        && len > max
    {
        out.push(Violation::LengthOutOfRange {
            field: name.to_string(),
            actual: len,
            min: rule.min_length,
            max: Some(max),
        });
    }

    // pattern
    if let Some(re) = &rule.pattern
        && !re.is_match(v)
    {
        out.push(Violation::PatternMismatch {
            field: name.to_string(),
            pattern: re.as_str().to_string(),
            actual: v.to_string(),
        });
    }

    // enum
    if let Some(allowed) = &rule.enum_values
        && !allowed.iter().any(|a| a == v)
    {
        out.push(Violation::NotInEnum {
            field: name.to_string(),
            actual: v.to_string(),
            allowed: allowed.clone(),
        });
    }
}

fn check_tags(out: &mut Vec<Violation>, name: &str, rule: &CompiledRule, tags: &[String]) {
    // type 不一致: array 以外を期待していたら mismatch
    if let Some(ft) = rule.field_type
        && ft != FieldType::Array
    {
        out.push(Violation::TypeMismatch {
            field: name.to_string(),
            expected: ft.as_str().to_string(),
            actual: "array".to_string(),
        });
        return;
    }

    // required && (empty && !allow_empty) → Missing
    if rule.required && tags.is_empty() && !rule.allow_empty {
        out.push(Violation::MissingRequired {
            field: name.to_string(),
        });
        return;
    }

    // length
    let len = tags.len();
    if let Some(min) = rule.min_length
        && len < min
    {
        out.push(Violation::LengthOutOfRange {
            field: name.to_string(),
            actual: len,
            min: Some(min),
            max: rule.max_length,
        });
    }
    if let Some(max) = rule.max_length
        && len > max
    {
        out.push(Violation::LengthOutOfRange {
            field: name.to_string(),
            actual: len,
            min: rule.min_length,
            max: Some(max),
        });
    }

    // enum: 各要素が enum に含まれているか
    if let Some(allowed) = &rule.enum_values {
        for t in tags {
            if !allowed.iter().any(|a| a == t) {
                out.push(Violation::NotInEnum {
                    field: name.to_string(),
                    actual: t.to_string(),
                    allowed: allowed.clone(),
                });
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(toml: &str) -> Schema {
        Schema::from_toml_str(toml).unwrap()
    }

    fn fm() -> Frontmatter {
        Frontmatter::default()
    }

    #[test]
    fn test_parse_minimal_schema() {
        let s = schema(
            r#"
            [fields.title]
            required = true
            type = "string"
            "#,
        );
        assert_eq!(s.fields.len(), 1);
        assert!(s.fields["title"].required);
        assert_eq!(s.fields["title"].field_type, Some(FieldType::String));
    }

    #[test]
    fn test_unknown_field_in_schema_is_rejected() {
        let err = Schema::from_toml_str(
            r#"
            [fields.bogus]
            required = true
            "#,
        )
        .expect_err("unknown field must fail");
        assert!(err.to_string().contains("unsupported field"));
    }

    #[test]
    fn test_invalid_regex_is_rejected() {
        let err = Schema::from_toml_str(
            r#"
            [fields.title]
            pattern = '[unclosed'
            "#,
        )
        .expect_err("invalid regex must fail");
        assert!(err.to_string().contains("invalid regex"));
    }

    #[test]
    fn test_integer_type_is_rejected_in_mvp() {
        // integer を silent pass させず compile で弾く (evaluator High #2)
        let err = Schema::from_toml_str(
            r#"
            [fields.title]
            type = "integer"
            "#,
        )
        .expect_err("integer must be rejected in MVP");
        assert!(err.to_string().contains("not implemented"));
        assert!(err.to_string().contains("pattern"));
    }

    #[test]
    fn test_date_type_is_rejected_in_mvp() {
        let err = Schema::from_toml_str(
            r#"
            [fields.date]
            type = "date"
            "#,
        )
        .expect_err("date must be rejected in MVP");
        assert!(err.to_string().contains("not implemented"));
    }

    #[test]
    fn test_pattern_on_array_is_rejected() {
        let err = Schema::from_toml_str(
            r#"
            [fields.tags]
            type = "array"
            pattern = "^foo"
            "#,
        )
        .expect_err("pattern on array must fail");
        assert!(err.to_string().contains("pattern"));
    }

    #[test]
    fn test_validate_missing_required() {
        let s = schema(
            r#"[fields.title]
required = true
type = "string""#,
        );
        let v = validate(&fm(), &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(&v[0], Violation::MissingRequired { field } if field == "title"));
    }

    #[test]
    fn test_validate_empty_string_is_missing_when_required() {
        let s = schema(
            r#"[fields.title]
required = true
type = "string""#,
        );
        let mut f = fm();
        f.title = Some(String::new());
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(&v[0], Violation::MissingRequired { .. }));
    }

    #[test]
    fn test_validate_empty_string_allowed_with_allow_empty() {
        let s = schema(
            r#"[fields.title]
required = true
type = "string"
allow_empty = true"#,
        );
        let mut f = fm();
        f.title = Some(String::new());
        let v = validate(&f, &s);
        assert!(
            v.is_empty(),
            "allow_empty must suppress empty missing, got {v:?}"
        );
    }

    #[test]
    fn test_validate_pattern_mismatch() {
        let s = schema(
            r#"[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'"#,
        );
        let mut f = fm();
        f.date = Some("2026/04/19".into()); // slashes, not dashes
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(&v[0], Violation::PatternMismatch { .. }));
    }

    #[test]
    fn test_validate_pattern_match_ok() {
        let s = schema(
            r#"[fields.date]
pattern = '^\d{4}-\d{2}-\d{2}$'"#,
        );
        let mut f = fm();
        f.date = Some("2026-04-19".into());
        let v = validate(&f, &s);
        assert!(v.is_empty());
    }

    #[test]
    fn test_validate_enum_miss() {
        let s = schema(
            r#"[fields.topic]
required = true
type = "string"
enum = ["mcp", "rag"]"#,
        );
        let mut f = fm();
        f.topic = Some("general".into());
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(&v[0], Violation::NotInEnum { .. }));
    }

    #[test]
    fn test_validate_tags_empty_required() {
        let s = schema(
            r#"[fields.tags]
required = true
type = "array"
min_length = 1"#,
        );
        let v = validate(&fm(), &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(&v[0], Violation::MissingRequired { field } if field == "tags"));
    }

    #[test]
    fn test_validate_tags_length_out_of_range() {
        let s = schema(
            r#"[fields.tags]
type = "array"
max_length = 2"#,
        );
        let mut f = fm();
        f.tags = vec!["a".into(), "b".into(), "c".into()];
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(
            &v[0],
            Violation::LengthOutOfRange {
                actual: 3,
                max: Some(2),
                ..
            }
        ));
    }

    #[test]
    fn test_validate_tags_enum_on_each_element() {
        let s = schema(
            r#"[fields.tags]
type = "array"
enum = ["mcp", "rag"]"#,
        );
        let mut f = fm();
        f.tags = vec!["mcp".into(), "random".into(), "rag".into()];
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(
            &v[0],
            Violation::NotInEnum { actual, .. } if actual == "random"
        ));
    }

    #[test]
    fn test_validate_type_mismatch_string_vs_array() {
        // title に array 型を指定すると Frontmatter 側 string と不一致
        let s = schema(
            r#"[fields.title]
type = "array""#,
        );
        let mut f = fm();
        f.title = Some("hi".into());
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1);
        assert!(matches!(
            &v[0],
            Violation::TypeMismatch { expected, actual, .. }
                if expected == "array" && actual == "string"
        ));
    }

    #[test]
    fn test_validate_multiple_violations() {
        let s = schema(
            r#"[fields.title]
required = true
type = "string"
min_length = 10

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'"#,
        );
        let mut f = fm();
        f.title = Some("hi".into()); // too short
        f.date = Some("bogus".into()); // no match
        let v = validate(&f, &s);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn test_violation_message_missing() {
        let v = Violation::MissingRequired {
            field: "title".into(),
        };
        assert!(v.message().contains("required"));
    }

    #[test]
    fn test_violation_message_enum() {
        let v = Violation::NotInEnum {
            field: "topic".into(),
            actual: "x".into(),
            allowed: vec!["a".into(), "b".into()],
        };
        let m = v.message();
        assert!(m.contains("topic"));
        assert!(m.contains("[a, b]"));
    }

    #[test]
    fn test_load_optional_missing_returns_none() {
        let p = std::env::temp_dir().join("kb-mcp-schema-nonexistent.toml");
        let _ = std::fs::remove_file(&p);
        let s = Schema::load_optional(&p).unwrap();
        assert!(s.is_none());
    }

    #[test]
    fn test_validate_ok_when_all_fields_valid() {
        let s = schema(
            r#"[fields.title]
required = true
type = "string"
min_length = 1

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.topic]
required = true
type = "string"
enum = ["mcp", "rag", "ai"]

[fields.tags]
required = true
type = "array"
min_length = 1"#,
        );
        let mut f = fm();
        f.title = Some("Hello".into());
        f.date = Some("2026-04-19".into());
        f.topic = Some("mcp".into());
        f.tags = vec!["one".into()];
        let v = validate(&f, &s);
        assert!(v.is_empty(), "expected no violations, got {v:?}");
    }
}
