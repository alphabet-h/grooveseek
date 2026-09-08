//! YAML frontmatter の構造規約を定義する TOML スキーマと
//! それに対するバリデータ。
//!
//! スキーマ記述例 (`groove-schema.toml`):
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
//! [`crate::schema::validate_document`] はその前段で、YAML が parse できなかった
//! 文書を違反 1 件 (`frontmatter_unparsed`) に畳む。CLI `groove validate`
//! サブコマンドは後者を呼ぶ。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;

use crate::parser::{FieldValue, Frontmatter, ParsedDocument};

// ---------------------------------------------------------------------------
// Schema types
// ---------------------------------------------------------------------------

/// `[options]` of `groove-schema.toml` (feature-57).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SchemaOptions {
    /// `false` makes a frontmatter key that no `[fields.*]` table names an
    /// `undeclared_field` violation. `groove validate --strict` has the same
    /// effect from the command line. The default is `true`: today's behavior.
    #[serde(default = "default_allow_unknown_fields")]
    pub allow_unknown_fields: bool,
}

fn default_allow_unknown_fields() -> bool {
    true
}

impl Default for SchemaOptions {
    fn default() -> Self {
        Self {
            allow_unknown_fields: default_allow_unknown_fields(),
        }
    }
}

/// `groove-schema.toml` のルート構造。
///
/// `[fields.<name>]` は任意の名前を受ける (feature-57)。`title` / `date` /
/// `topic` / `depth` / `tags` は parser が専用 field に持つが、schema の側では
/// 他の名前と同じ rule で扱う。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RawSchema {
    #[serde(default)]
    pub fields: BTreeMap<String, FieldRule>,
    #[serde(default)]
    pub options: SchemaOptions,
}

/// 個々のフィールドに対する検証ルール。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    /// `true` なら欠落 (None) 時に MissingRequired 違反。
    #[serde(default)]
    pub required: bool,
    /// `"string"` / `"array"` / `"date"` / `"integer"`。
    /// 現状 Frontmatter は `string` と `array<string>` のみだが、スキーマ側は
    /// ユーザの書き方を広めに受け入れ、loader が妥当性を判断する。
    #[serde(default, rename = "type")]
    pub field_type: Option<FieldType>,
    /// 正規表現 (Rust `regex` 互換)。string なら値全体、array なら各要素に
    /// 適用 (enum と同じ、1.8.0 から / #252)。
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
    /// From `[options].allow_unknown_fields`; `groove validate --strict`
    /// sets it to `false` after loading (feature-57).
    pub allow_unknown_fields: bool,
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
    /// 不正な regex や未実装の type はここで reject する。
    pub fn from_toml_str(src: &str) -> Result<Self> {
        let raw: RawSchema = toml::from_str(src).context("failed to parse schema TOML")?;
        Self::compile(raw)
    }

    /// Make a frontmatter key no `[fields.*]` table names a violation: the
    /// flag form of `[options].allow_unknown_fields = false` (feature-57).
    ///
    /// It only ever tightens. A schema that already asks for this is
    /// unaffected, and there is no call that loosens it back -- `--strict` has
    /// no `--no-strict`, so the schema file is the only place `true` is set.
    pub fn require_declared_fields(&mut self) {
        self.allow_unknown_fields = false;
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
        Ok(Schema {
            fields: out,
            allow_unknown_fields: raw.options.allow_unknown_fields,
        })
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
    /// The file's `---` block was found but its YAML was refused (#251), so
    /// there is no frontmatter to hold the schema against. Reported instead
    /// of the schema violations, not beside them (#252). `field` is always
    /// [`FRONTMATTER_FIELD`]: the block itself, not a schema field.
    FrontmatterUnparsed {
        field: String,
        reason: String,
    },
    /// A key the frontmatter carries that no `[fields.*]` table names,
    /// reported only when `[options].allow_unknown_fields = false` or
    /// `groove validate --strict` (feature-57). One per key, in key order.
    UndeclaredField {
        field: String,
    },
}

/// The `field` a [`Violation::FrontmatterUnparsed`] carries. Every violation
/// object has a `field` key, so a consumer reading it never sees null.
pub const FRONTMATTER_FIELD: &str = "frontmatter";

impl Violation {
    pub fn field(&self) -> &str {
        match self {
            Violation::MissingRequired { field } => field,
            Violation::TypeMismatch { field, .. } => field,
            Violation::PatternMismatch { field, .. } => field,
            Violation::NotInEnum { field, .. } => field,
            Violation::LengthOutOfRange { field, .. } => field,
            Violation::FrontmatterUnparsed { field, .. } => field,
            Violation::UndeclaredField { field } => field,
        }
    }

    /// 人間向けの 1 行メッセージ。
    pub fn message(&self) -> String {
        match self {
            Violation::FrontmatterUnparsed { field, reason } => {
                // serde_yaml_bw の reason は複数行になり得る。text format は
                // `\n` を置換しないので、ここで 1 行に畳む。
                let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("{field} could not be parsed as YAML: {reason}")
            }
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
            Violation::UndeclaredField { field } => {
                format!("{field} is not declared in the schema")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// `Frontmatter` を `Schema` に照らして違反リストを返す。空リストなら OK。
///
/// 5 名は専用 field から、それ以外は [`Frontmatter::extra`] から引く (feature-57)。
/// `schema.allow_unknown_fields` が false なら、[`Frontmatter::extra`] にあって schema に
/// 無い key を [`Violation::UndeclaredField`] として key 順に足す。`title` / `date` /
/// `topic` / `depth` / `tags` は parser がそれぞれの field へ振り分けるので
/// [`Frontmatter::extra`] に現れず、schema に無くても undeclared にならない。
pub fn validate(fm: &Frontmatter, schema: &Schema) -> Vec<Violation> {
    let mut out = Vec::new();

    for (name, rule) in &schema.fields {
        match name.as_str() {
            "title" => check_string(&mut out, name, rule, fm.title.as_deref()),
            "date" => check_string(&mut out, name, rule, fm.date.as_deref()),
            "topic" => check_string(&mut out, name, rule, fm.topic.as_deref()),
            "depth" => check_string(&mut out, name, rule, fm.depth.as_deref()),
            "tags" => check_tags(&mut out, name, rule, &fm.tags),
            _ => check_extra(&mut out, name, rule, fm.extra.get(name)),
        }
    }

    if !schema.allow_unknown_fields {
        for key in fm.extra.keys() {
            if !schema.fields.contains_key(key) {
                out.push(Violation::UndeclaredField { field: key.clone() });
            }
        }
    }

    out
}

/// Validate a parsed Markdown document. A `---` block the parser refused
/// ([`ParsedDocument::frontmatter_error`] is `Some`) is one [`Violation::FrontmatterUnparsed`]
/// and the schema is not applied -- the frontmatter behind it is the parser's
/// placeholder (empty fields, the `frontmatter:unparsed` tag), and blaming
/// those would name the tag rather than the YAML (#251, #252). Otherwise this
/// is [`validate`] on [`ParsedDocument::frontmatter`].
pub fn validate_document(doc: &ParsedDocument, schema: &Schema) -> Vec<Violation> {
    match &doc.frontmatter_error {
        Some(reason) => vec![Violation::FrontmatterUnparsed {
            field: FRONTMATTER_FIELD.to_string(),
            reason: reason.clone(),
        }],
        None => validate(&doc.frontmatter, schema),
    }
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

    // pattern: 各要素が regex にマッチするか (check_string と同じく enum の前)
    if let Some(re) = &rule.pattern {
        for t in tags {
            if !re.is_match(t) {
                out.push(Violation::PatternMismatch {
                    field: name.to_string(),
                    pattern: re.as_str().to_string(),
                    actual: t.to_string(),
                });
            }
        }
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

/// A rule on a key the parser holds in [`Frontmatter::extra`] (feature-57). With no `type`
/// the value's own shape picks the path: a scalar is checked like a string
/// field, a list like `tags`. A mapping, or a sequence holding a non-scalar,
/// is opaque -- it satisfies `required` and nothing else, and any rule that
/// would read the value is one `type_mismatch` naming the shape.
///
/// A null ([`FieldValue::NULL`]) is not that: `status:` with nothing after it
/// is the key without a value, so it counts as absent for every rule and
/// `required` catches it the way it catches a blank `title:`. The key stays in
/// [`Frontmatter::extra`], so strict still reports it when no `[fields.*]` table names it.
fn check_extra(
    out: &mut Vec<Violation>,
    name: &str,
    rule: &CompiledRule,
    value: Option<&FieldValue>,
) {
    match value {
        None | Some(FieldValue::Other(FieldValue::NULL)) => {
            if rule.required {
                out.push(Violation::MissingRequired {
                    field: name.to_string(),
                });
            }
        }
        Some(FieldValue::Scalar(s)) => check_string(out, name, rule, Some(s)),
        Some(FieldValue::List(items)) => check_tags(out, name, rule, items),
        Some(other @ FieldValue::Other(_)) => {
            let reads_the_value = rule.field_type.is_some()
                || rule.pattern.is_some()
                || rule.enum_values.is_some()
                || rule.min_length.is_some()
                || rule.max_length.is_some();
            if reads_the_value {
                out.push(Violation::TypeMismatch {
                    field: name.to_string(),
                    expected: rule
                        .field_type
                        .map(|t| t.as_str().to_string())
                        .unwrap_or_else(|| "string or array".to_string()),
                    actual: other.shape().to_string(),
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

    /// The shipped template, as shipped.
    const SHIPPED_EXAMPLE: &str = include_str!("../groove-schema.toml.example");

    /// A template an operator copies has to compile. It advertised
    /// `type = "integer"` and `type = "date"` for eight releases, both of which
    /// this module refuses — so the first thing anyone following the comment
    /// would meet is a schema that will not load.
    ///
    /// The body was always fine; the comment above it was the lie. That is why
    /// the next test reads the comment rather than only running the body.
    #[test]
    fn the_shipped_schema_template_compiles() {
        Schema::from_toml_str(SHIPPED_EXAMPLE)
            .expect("groove-schema.toml.example must compile exactly as shipped");
    }

    /// Every spelling the template's `type` line offers must be one that
    /// compiles. The list is read out of the comment rather than restated
    /// here — restating it is what let the two drift apart.
    #[test]
    fn every_type_the_template_advertises_is_one_that_compiles() {
        let line = SHIPPED_EXAMPLE
            .lines()
            .find(|l| l.trim_start().starts_with("#") && l.contains("type") && l.contains(':'))
            .expect("the template documents its `type` values");
        let advertised: Vec<&str> = line
            .split('"')
            .skip(1)
            .step_by(2)
            .filter(|s| !s.is_empty())
            .collect();
        assert!(
            !advertised.is_empty(),
            "no quoted type names on {line:?}; the extraction, not the template, is wrong"
        );
        for ty in &advertised {
            let src = format!("[fields.title]\nrequired = true\ntype = \"{ty}\"\n");
            Schema::from_toml_str(&src)
                .unwrap_or_else(|e| panic!("the template offers type = {ty:?}, which fails: {e}"));
        }
    }

    /// The other half: the two the template stopped offering are still refused,
    /// so the comment has a reason to keep saying so. When one of them is
    /// implemented this fails, which is the moment to put it back on the line.
    #[test]
    fn the_types_the_template_warns_about_are_still_refused() {
        for ty in ["integer", "date"] {
            let src = format!("[fields.title]\nrequired = true\ntype = \"{ty}\"\n");
            let err = Schema::from_toml_str(&src)
                .expect_err("frontmatter is held as strings, so neither is implemented");
            assert!(
                format!("{err:#}").contains("not implemented"),
                "the refusal has to say why, got {err:#}"
            );
        }
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

    /// feature-57: a schema names any key it wants. The five the parser
    /// stores in their own fields are not special here.
    #[test]
    fn test_any_field_name_is_accepted() {
        let s = schema(
            r#"
            [fields.status]
            enum = ["active", "deprecated"]

            [fields.team]

            [fields."last modified"]
            pattern = '^\d{4}-'
            "#,
        );
        assert_eq!(s.fields.len(), 3);
        assert!(
            !s.fields["team"].required,
            "an empty table declares, it does not require"
        );
        assert!(s.fields["team"].field_type.is_none());
        assert!(s.fields["last modified"].pattern.is_some());
    }

    #[test]
    fn test_options_default_allows_unknown_fields() {
        let s = schema("[fields.title]\nrequired = true\n");
        assert!(s.allow_unknown_fields, "the default is today's behavior");
    }

    #[test]
    fn test_options_allow_unknown_fields_false_is_read() {
        let s = schema("[options]\nallow_unknown_fields = false\n\n[fields.title]\n");
        assert!(!s.allow_unknown_fields);
    }

    #[test]
    fn test_options_unknown_key_is_rejected() {
        let err = Schema::from_toml_str("[options]\nstrict = true\n")
            .expect_err("an unknown key under [options] is a load error");
        assert!(
            format!("{err:#}").contains("strict"),
            "the error names the key: {err:#}"
        );
    }

    /// The reporter's `known = true` proposal is not a rule key; a schema
    /// carrying it still fails to load, as any unknown rule key does.
    #[test]
    fn test_known_is_not_a_rule_key() {
        Schema::from_toml_str("[fields.status]\nknown = true\n")
            .expect_err("`known` is not a rule");
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

    /// Until 1.8.0 this schema was refused with "`pattern` is only valid for
    /// string-typed fields" (#252). It now compiles, and the pattern is held
    /// for [`check_tags`] to apply to every element.
    #[test]
    fn test_pattern_on_array_is_accepted() {
        let s = Schema::from_toml_str(
            r#"
            [fields.tags]
            type = "array"
            pattern = "^foo"
            "#,
        )
        .expect("pattern on an array field compiles since 1.8.0 (#252)");
        assert!(s.fields["tags"].pattern.is_some());
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

    /// `pattern` on an array field is applied to every element, the way
    /// `enum` is: one [`Violation::PatternMismatch`] per offending element, in tag order,
    /// carrying the element as `actual` (#252).
    #[test]
    fn test_validate_tags_pattern_on_each_element() {
        let s = schema(
            r#"[fields.tags]
type = "array"
pattern = '^[a-z-]+$'"#,
        );
        let mut f = fm();
        f.tags = vec!["mcp".into(), "Bad Tag".into(), "rag".into(), "x_y".into()];
        let v = validate(&f, &s);
        assert_eq!(v.len(), 2, "one violation per offending element, got {v:?}");
        assert!(matches!(
            &v[0],
            Violation::PatternMismatch { field, pattern, actual }
                if field == "tags" && pattern == "^[a-z-]+$" && actual == "Bad Tag"
        ));
        assert!(matches!(
            &v[1],
            Violation::PatternMismatch { actual, .. } if actual == "x_y"
        ));
    }

    #[test]
    fn test_validate_tags_pattern_match_ok() {
        let s = schema(
            r#"[fields.tags]
type = "array"
pattern = '^[a-z-]+$'"#,
        );
        let mut f = fm();
        f.tags = vec!["mcp".into(), "status-active".into()];
        let v = validate(&f, &s);
        assert!(v.is_empty(), "every element matches, got {v:?}");
    }

    /// `[fields.tags] pattern = ...` without a `type` key compiled before
    /// 1.8.0 and was silently ignored. It is applied now.
    #[test]
    fn test_validate_tags_pattern_applies_without_type_key() {
        let s = schema(
            r#"[fields.tags]
pattern = '^[a-z-]+$'"#,
        );
        let mut f = fm();
        f.tags = vec!["ok".into(), "Not OK".into()];
        let v = validate(&f, &s);
        assert_eq!(v.len(), 1, "got {v:?}");
        assert!(matches!(
            &v[0],
            Violation::PatternMismatch { actual, .. } if actual == "Not OK"
        ));
    }

    /// An element that fails both checks yields both violations, pattern
    /// first -- the order [`check_string`] uses for a string field.
    #[test]
    fn test_validate_tags_pattern_then_enum_for_one_element() {
        let s = schema(
            r#"[fields.tags]
type = "array"
pattern = '^[a-z]+$'
enum = ["mcp"]"#,
        );
        let mut f = fm();
        f.tags = vec!["Bad".into()];
        let v = validate(&f, &s);
        assert_eq!(v.len(), 2, "got {v:?}");
        assert!(matches!(&v[0], Violation::PatternMismatch { .. }));
        assert!(matches!(&v[1], Violation::NotInEnum { .. }));
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
        let p = std::env::temp_dir().join("groove-schema-nonexistent.toml");
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

    // -----------------------------------------------------------------------
    // validate_document: a refused frontmatter block is one violation (#252)
    // -----------------------------------------------------------------------

    fn parse_md(raw: &str) -> crate::parser::ParsedDocument {
        use crate::parser::Parser as _;
        crate::parser::MarkdownParser.parse(raw, "doc.md", &[])
    }

    /// The schema every [`validate_document`] test below is held against:
    /// each rule would fire on the empty metadata the parser leaves behind a
    /// refused block, so any of them leaking through is visible.
    fn strict_schema() -> Schema {
        schema(
            r#"[fields.title]
required = true
type = "string"

[fields.date]
required = true
type = "string"
pattern = '^\d{4}-\d{2}-\d{2}$'

[fields.tags]
required = true
type = "array"
pattern = '^[a-z-]+$'
enum = ["mcp"]"#,
        )
    }

    /// Since 1.7.0 the parser tags a document whose YAML was refused with
    /// `frontmatter:unparsed` and leaves every other field empty (#251), so
    /// [`validate`] on that frontmatter blamed the tag and the missing title.
    /// [`validate_document`] reports the refusal itself, once, instead.
    #[test]
    fn test_validate_document_broken_yaml_is_one_violation() {
        let doc = parse_md("---\ntitle: [unclosed\n---\n# body\n");
        assert!(doc.frontmatter_error.is_some(), "fixture must be refused");
        let v = validate_document(&doc, &strict_schema());
        assert_eq!(v.len(), 1, "one violation for the block, got {v:?}");
        assert!(matches!(
            &v[0],
            Violation::FrontmatterUnparsed { field, reason }
                if field == "frontmatter" && !reason.is_empty()
        ));
    }

    #[test]
    fn test_validate_document_valid_yaml_equals_validate() {
        let doc =
            parse_md("---\ntitle: Hello\ndate: \"2026-09-08\"\ntags: [mcp, Bad]\n---\n# body\n");
        assert!(doc.frontmatter_error.is_none());
        let s = strict_schema();
        let v = validate_document(&doc, &s);
        assert_eq!(v, validate(&doc.frontmatter, &s));
        assert!(
            v.iter()
                .any(|x| matches!(x, Violation::PatternMismatch { actual, .. } if actual == "Bad")),
            "the schema is applied as before, got {v:?}"
        );
    }

    /// No block, and a block without a closing fence, are not parse errors:
    /// the schema is applied to the (empty) frontmatter as before.
    #[test]
    fn test_validate_document_absent_and_unterminated_block_use_schema() {
        for raw in ["# no frontmatter\n", "---\ntitle: x\nno closing fence\n"] {
            let doc = parse_md(raw);
            assert!(doc.frontmatter_error.is_none(), "{raw:?}");
            let v = validate_document(&doc, &strict_schema());
            assert!(
                v.iter()
                    .any(|x| matches!(x, Violation::MissingRequired { field } if field == "title")),
                "{raw:?}: schema applies, got {v:?}"
            );
            assert!(
                !v.iter()
                    .any(|x| matches!(x, Violation::FrontmatterUnparsed { .. })),
                "{raw:?}: not a refused block, got {v:?}"
            );
        }
    }

    /// The tag is frontmatter, so valid YAML can declare it by hand. That is
    /// not a refused block: the schema applies and a tag pattern flags it.
    #[test]
    fn test_validate_document_hand_written_unparsed_tag_is_checked_by_schema() {
        let doc = parse_md("---\ntags: [\"frontmatter:unparsed\"]\n---\n");
        assert!(doc.frontmatter_error.is_none());
        let s = schema(
            r#"[fields.tags]
type = "array"
pattern = '^[a-z-]+$'"#,
        );
        let v = validate_document(&doc, &s);
        assert_eq!(v.len(), 1, "got {v:?}");
        assert!(matches!(
            &v[0],
            Violation::PatternMismatch { actual, .. } if actual == "frontmatter:unparsed"
        ));
    }

    /// The parser's reason may span lines; the violation is one line in
    /// every output format, and its JSON carries the documented keys.
    #[test]
    fn test_violation_frontmatter_unparsed_message_is_one_line_and_serialises() {
        let doc = parse_md("---\ntitle: [unclosed\n---\n");
        let v = validate_document(&doc, &strict_schema());
        let m = v[0].message();
        assert!(!m.contains('\n'), "one line: {m:?}");
        assert!(
            m.starts_with("frontmatter could not be parsed as YAML: "),
            "{m:?}"
        );
        let json = serde_json::to_value(&v[0]).unwrap();
        assert_eq!(json["kind"], "frontmatter_unparsed");
        assert_eq!(json["field"], "frontmatter");
        assert!(json["reason"].as_str().is_some_and(|r| !r.is_empty()));

        let multi = Violation::FrontmatterUnparsed {
            field: "frontmatter".into(),
            reason: "line one\n  line two".into(),
        };
        assert_eq!(
            multi.message(),
            "frontmatter could not be parsed as YAML: line one line two"
        );
    }

    // -----------------------------------------------------------------------
    // feature-57: extra fields and strict mode
    // -----------------------------------------------------------------------

    fn fm_with(extra: &[(&str, FieldValue)]) -> Frontmatter {
        Frontmatter {
            title: Some("T".into()),
            extra: extra
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            ..Frontmatter::default()
        }
    }

    /// Acceptance 1: an empty table declares the key; a document with it and
    /// one without it both pass.
    #[test]
    fn test_empty_table_is_declared_not_required() {
        let s = schema("[fields.status]\n");
        assert!(validate(&fm_with(&[("status", FieldValue::Scalar("x".into()))]), &s).is_empty());
        assert!(validate(&fm_with(&[]), &s).is_empty());
    }

    /// Acceptance 2: an extra scalar takes `enum`.
    #[test]
    fn test_extra_scalar_enum() {
        let s = schema("[fields.status]\nenum = [\"active\", \"deprecated\"]\n");
        let v = validate(
            &fm_with(&[("status", FieldValue::Scalar("retired".into()))]),
            &s,
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(
            matches!(&v[0], Violation::NotInEnum { field, actual, .. } if field == "status" && actual == "retired")
        );
        assert!(
            validate(
                &fm_with(&[("status", FieldValue::Scalar("active".into()))]),
                &s
            )
            .is_empty()
        );
    }

    /// Acceptance 3: with no `type`, a list takes element-wise `enum` and a
    /// scalar takes the same `enum` whole.
    #[test]
    fn test_extra_untyped_rule_follows_the_value_shape() {
        let s = schema("[fields.environment]\nenum = [\"dev\", \"test\", \"prod\"]\n");
        let list = FieldValue::List(vec!["dev".into(), "staging".into()]);
        let v = validate(&fm_with(&[("environment", list)]), &s);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(&v[0], Violation::NotInEnum { actual, .. } if actual == "staging"));
        assert!(
            validate(
                &fm_with(&[("environment", FieldValue::Scalar("dev".into()))]),
                &s
            )
            .is_empty()
        );
    }

    /// Acceptance 3, pattern half: element-wise on a list.
    #[test]
    fn test_extra_list_pattern_is_element_wise() {
        let s = schema("[fields.environment]\ntype = \"array\"\npattern = '^[a-z]+$'\n");
        let list = FieldValue::List(vec!["dev".into(), "Prod".into(), "x1".into()]);
        let v = validate(&fm_with(&[("environment", list)]), &s);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(
            v.iter()
                .all(|x| matches!(x, Violation::PatternMismatch { .. }))
        );
    }

    /// Acceptance 4: a boolean is the string it prints as.
    #[test]
    fn test_extra_bool_is_checked_as_a_string() {
        let s = schema("[fields.environment_declared]\nenum = [\"true\", \"false\"]\n");
        assert!(
            validate(
                &fm_with(&[("environment_declared", FieldValue::Scalar("false".into()))]),
                &s
            )
            .is_empty()
        );
        let v = validate(
            &fm_with(&[("environment_declared", FieldValue::Scalar("no".into()))]),
            &s,
        );
        assert_eq!(v.len(), 1, "{v:?}");
    }

    /// Acceptance 5: an opaque shape satisfies `required` and nothing else.
    #[test]
    fn test_extra_other_is_present_but_not_checkable() {
        let only_required = schema("[fields.meta]\nrequired = true\n");
        assert!(
            validate(
                &fm_with(&[("meta", FieldValue::Other("mapping"))]),
                &only_required
            )
            .is_empty()
        );

        let with_pattern = schema("[fields.meta]\npattern = '.'\n");
        let v = validate(
            &fm_with(&[("meta", FieldValue::Other("mapping"))]),
            &with_pattern,
        );
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(
            &v[0],
            Violation::TypeMismatch { field, expected, actual }
                if field == "meta" && expected == "string or array" && actual == "mapping"
        ));

        let typed = schema("[fields.meta]\ntype = \"string\"\nenum = [\"a\"]\nmin_length = 1\n");
        let v = validate(&fm_with(&[("meta", FieldValue::Other("mapping"))]), &typed);
        assert_eq!(v.len(), 1, "one type_mismatch, not one per rule: {v:?}");
        assert!(
            matches!(&v[0], Violation::TypeMismatch { expected, actual, .. } if expected == "string" && actual == "mapping")
        );
    }

    /// `binary` is opaque like `mapping`: present for `required`, unreadable
    /// for everything else, and named as `actual` when a rule reads it.
    #[test]
    fn test_extra_binary_is_opaque() {
        let binary = || fm_with(&[("blob", FieldValue::Other("binary"))]);

        let only_required = schema("[fields.blob]\nrequired = true\n");
        assert!(validate(&binary(), &only_required).is_empty());

        let with_pattern = schema("[fields.blob]\npattern = '.'\n");
        let v = validate(&binary(), &with_pattern);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(
            &v[0],
            Violation::TypeMismatch { field, expected, actual }
                if field == "blob" && expected == "string or array" && actual == "binary"
        ));
    }

    /// A null is the key written with no value, so every rule reads it as
    /// absent: `required` catches a blank `meta:` the way it catches a blank
    /// `title:`, and a rule that would read a value has nothing to read.
    #[test]
    fn test_extra_null_counts_as_absent_but_is_still_a_key() {
        let null = || fm_with(&[("meta", FieldValue::Other("null"))]);

        let required = schema("[fields.meta]\nrequired = true\n");
        let v = validate(&null(), &required);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(&v[0], Violation::MissingRequired { field } if field == "meta"));

        let with_pattern = schema("[fields.meta]\npattern = '.'\n");
        assert!(
            validate(&null(), &with_pattern).is_empty(),
            "there is no value to hold a pattern against"
        );

        // Absent for the rules, present for strict: the key is in the block.
        let mut strict = schema("[fields.title]\n");
        strict.require_declared_fields();
        let v = validate(&null(), &strict);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(&v[0], Violation::UndeclaredField { field } if field == "meta"));
    }

    /// Every named field is read from its own [`Frontmatter`] field, and none of
    /// them can be undeclared -- the parser never puts one in [`Frontmatter::extra`], so the
    /// strict loop has nothing to exempt. One case per match arm, `depth`
    /// included.
    #[test]
    fn test_each_named_field_is_read_from_its_own_field() {
        let cases = [
            (
                "title",
                Frontmatter {
                    title: Some("T".into()),
                    ..Frontmatter::default()
                },
            ),
            (
                "date",
                Frontmatter {
                    date: Some("2026-09-09".into()),
                    ..Frontmatter::default()
                },
            ),
            (
                "topic",
                Frontmatter {
                    topic: Some("mcp".into()),
                    ..Frontmatter::default()
                },
            ),
            (
                "depth",
                Frontmatter {
                    depth: Some("2".into()),
                    ..Frontmatter::default()
                },
            ),
            (
                "tags",
                Frontmatter {
                    tags: vec!["a".into()],
                    ..Frontmatter::default()
                },
            ),
        ];
        for (name, f) in cases {
            assert!(f.extra.is_empty(), "{name}: the fixture holds no extras");

            let mut s = schema(&format!("[fields.{name}]\nrequired = true\n"));
            assert!(
                validate(&f, &s).is_empty(),
                "{name} is satisfied by its own field, got {:?}",
                validate(&f, &s)
            );

            s.require_declared_fields();
            assert!(
                validate(&f, &s).is_empty(),
                "{name} is never undeclared, got {:?}",
                validate(&f, &s)
            );
        }
    }

    /// `--strict` reaches the schema through this, and it only tightens: a
    /// schema that already asks for it is unchanged.
    #[test]
    fn test_require_declared_fields_only_tightens() {
        let mut s = schema("[fields.title]\n");
        assert!(s.allow_unknown_fields);
        s.require_declared_fields();
        assert!(!s.allow_unknown_fields);
        s.require_declared_fields();
        assert!(!s.allow_unknown_fields);

        let mut already = schema("[options]\nallow_unknown_fields = false\n");
        already.require_declared_fields();
        assert!(!already.allow_unknown_fields);
    }

    /// Declared `type` against the opposite shape reports the same
    /// `type_mismatch` the five named fields already report.
    #[test]
    fn test_extra_declared_type_mismatch() {
        let s = schema("[fields.a]\ntype = \"string\"\n\n[fields.b]\ntype = \"array\"\n");
        let v = validate(
            &fm_with(&[
                ("a", FieldValue::List(vec![])),
                ("b", FieldValue::Scalar("x".into())),
            ]),
            &s,
        );
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(
            matches!(&v[0], Violation::TypeMismatch { field, expected, actual } if field == "a" && expected == "string" && actual == "array")
        );
        assert!(
            matches!(&v[1], Violation::TypeMismatch { field, expected, actual } if field == "b" && expected == "array" && actual == "string")
        );
    }

    /// A declared-but-absent extra is `missing_required` only when required;
    /// a declared `type = "array"` does not turn absence into a type error.
    #[test]
    fn test_extra_absent() {
        let s = schema(
            "[fields.a]\nrequired = true\ntype = \"array\"\n\n[fields.b]\ntype = \"array\"\n",
        );
        let v = validate(&fm_with(&[]), &s);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(&v[0], Violation::MissingRequired { field } if field == "a"));
    }

    /// Acceptance 6 and 7: strict reports each undeclared key once, in key
    /// order; the five named fields are never undeclared.
    #[test]
    fn test_strict_reports_each_undeclared_key() {
        let mut s = schema("[fields.status]\n");
        let fm = Frontmatter {
            title: Some("T".into()),
            tags: vec!["x".into()],
            extra: [
                ("team".to_string(), FieldValue::Scalar("p".into())),
                ("status".to_string(), FieldValue::Scalar("active".into())),
                ("meta".to_string(), FieldValue::Other("mapping")),
            ]
            .into_iter()
            .collect(),
            ..Frontmatter::default()
        };
        assert!(
            validate(&fm, &s).is_empty(),
            "not strict: nothing to report"
        );

        s.allow_unknown_fields = false;
        let v = validate(&fm, &s);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(matches!(&v[0], Violation::UndeclaredField { field } if field == "meta"));
        assert!(matches!(&v[1], Violation::UndeclaredField { field } if field == "team"));
    }

    #[test]
    fn test_strict_from_options_needs_no_flag() {
        let s = schema("[options]\nallow_unknown_fields = false\n\n[fields.title]\n");
        let v = validate(&fm_with(&[("team", FieldValue::Scalar("p".into()))]), &s);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(&v[0], Violation::UndeclaredField { field } if field == "team"));
    }

    /// Acceptance 8: a refused block is one `frontmatter_unparsed`, strict or
    /// not -- there are no keys to call undeclared.
    #[test]
    fn test_strict_broken_yaml_is_still_one_violation() {
        let mut s = strict_schema();
        s.allow_unknown_fields = false;
        let doc = parse_md("---\ntitle: [unclosed\nteam: p\n---\n# body\n");
        let v = validate_document(&doc, &s);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(matches!(&v[0], Violation::FrontmatterUnparsed { .. }));
    }

    #[test]
    fn test_undeclared_field_json_and_message() {
        let v = Violation::UndeclaredField {
            field: "team".into(),
        };
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["kind"], "undeclared_field");
        assert_eq!(json["field"], "team");
        assert_eq!(v.field(), "team");
        assert_eq!(v.message(), "team is not declared in the schema");
    }
}
