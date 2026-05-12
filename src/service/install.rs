//! Install orchestration for kb-mcp service backends.
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

/// kb_path を解決 (spec § Q1 c-3 hybrid):
/// 1. `--kb-path` flag (= Some(flag)) が指定されたらそれ
/// 2. それ以外で toml_path が指定されたら toml の `[index].kb_path` を読む
/// 3. 両方 None なら error
pub fn resolve_kb_path(
    flag: Option<PathBuf>,
    toml_path: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p);
    }
    let Some(toml_path) = toml_path else {
        return Err(anyhow!(
            "kb_path が解決できません: --kb-path flag を指定するか、kb-mcp.toml に [index].kb_path を書いてください"
        ));
    };
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("kb-mcp.toml 読込失敗: {}", toml_path.display()))?;
    let parsed: toml::Value = toml::from_str(&content)?;
    let kb_path = parsed
        .get("index")
        .and_then(|v| v.get("kb_path"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{} に [index].kb_path がありません", toml_path.display()))?;
    Ok(PathBuf::from(kb_path))
}
