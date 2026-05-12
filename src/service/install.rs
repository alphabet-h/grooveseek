//! Install orchestration for kb-mcp service backends.
use crate::service::{InstallContext, backend, resolve_config_home, validate_service_name};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

pub struct InstallParams {
    pub service_name: String,
    pub kb_path: Option<PathBuf>,
    pub bind: String,
    pub auto_start: bool,
    pub force: bool,
    pub i_know_non_loopback: bool,
}

pub fn run(params: InstallParams) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;

    if !is_loopback_addr(&params.bind) && !params.i_know_non_loopback {
        return Err(anyhow!(
            "bind={} は non-loopback です。kb-mcp は auth を持ちません — \
             untrusted network での公開は危険。確認して進める場合は --i-know を付けて再実行してください。",
            params.bind
        ));
    }

    let config_home = resolve_config_home(&name)?;
    std::fs::create_dir_all(&config_home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_home, std::fs::Permissions::from_mode(0o700))?;
    }

    let toml_path = config_home.join("kb-mcp.toml");
    if toml_path.exists() && !params.force {
        return Err(anyhow!(
            "kb-mcp.toml が既存: {} (--force で上書き)",
            toml_path.display()
        ));
    }
    let kb_path = resolve_kb_path(
        params.kb_path,
        Some(toml_path.clone()).filter(|p| p.exists()),
    )?;
    write_toml(&toml_path, &kb_path, &params.bind)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&toml_path, std::fs::Permissions::from_mode(0o600))?;
    }

    let ctx = InstallContext {
        service_name: name.clone(),
        kb_path,
        bind: params.bind,
        config_home: config_home.clone(),
        binary_path: std::env::current_exe().context("std::env::current_exe() 解決失敗")?,
        auto_start: params.auto_start,
        force: params.force,
    };

    backend().install(&ctx)?;
    eprintln!(
        "Service '{}' installed (config_home: {}).",
        name,
        config_home.display()
    );
    Ok(())
}

fn is_loopback_addr(s: &str) -> bool {
    s.starts_with("127.") || s.starts_with("[::1]") || s.starts_with("localhost")
}

fn write_toml(path: &std::path::Path, kb_path: &std::path::Path, bind: &str) -> Result<()> {
    // TOML literal strings (single quotes) avoid \U escape issues on Windows paths.
    let content = format!(
        "[index]\nkb_path = '{kb}'\n\n[transport.http]\nbind = '{bind}'\n",
        kb = kb_path.display(),
        bind = bind,
    );
    std::fs::write(path, content)?;
    Ok(())
}

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
