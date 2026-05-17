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
    /// (feature-44 PR-3, Windows-only) Also install the kb-mcp-tray.exe
    /// shell:startup shortcut. `force` doubles as the tray duplicate-check
    /// override.
    pub with_tray: bool,
}

pub fn run(params: InstallParams) -> Result<()> {
    let name = validate_service_name(&params.service_name).map_err(|e| anyhow!(e))?;

    // codex P2 round 3 on PR #56: validate bind as SocketAddr at install time
    // instead of waiting for the daemon to fail at startup. A typo like
    // "localhost:3100" or a missing port like "127.0.0.1" passes is_loopback
    // but Transport::resolve() rejects it later — by which point the user has
    // already registered the service and would not see the error.
    let _: std::net::SocketAddr = params.bind.parse().with_context(|| {
        format!(
            "--bind '{}' is not a valid socket address (e.g. '127.0.0.1:3100')",
            params.bind
        )
    })?;

    if !is_loopback_addr(&params.bind) && !params.i_know_non_loopback {
        return Err(anyhow!(
            "bind={} は non-loopback です。kb-mcp は auth を持ちません — \
             untrusted network での公開は危険。確認して進める場合は --i-know を付けて再実行してください。",
            params.bind
        ));
    }
    // (codex P2 round 3 on PR #57, design clarification) Loopback-only admin
    // is by spec § 7 — even on non-loopback bind, /ui + /api/admin/status +
    // /api/search reject Host headers outside the loopback aliases + bind
    // addr. Warn the user that LAN browsers will see 403 on admin paths so
    // they expect to SSH to the host (or use http://127.0.0.1:<port>/ui) for
    // the WebUI even when /mcp is exposed on LAN.
    if !is_loopback_addr(&params.bind) {
        eprintln!(
            "Note: admin endpoints (/ui, /api/admin/status, /api/search) are \
             loopback-only by design. Browsers on the LAN will get 403 from \
             these paths even though /mcp accepts the same Host. Use \
             http://127.0.0.1:<port>/ui (locally) or SSH to the host for the WebUI."
        );
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
    // Relative `--kb-path` values must be normalised against the install-time
    // CWD before persisting to `kb-mcp.toml`. The installed service runs with
    // `WorkingDirectory=config_home`, and `Config::load_from` resolves
    // relative `kb_path` against the directory containing the toml — so a
    // raw relative path would point the daemon at `<config_home>/<rel>`
    // instead of the user's actual KB. canonicalize() also resolves symlinks
    // which is desirable here (= snapshot the install-time target).
    let kb_path = std::fs::canonicalize(&kb_path).with_context(|| {
        format!(
            "kb_path を絶対パスに正規化できませんでした: {}",
            kb_path.display()
        )
    })?;
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

    // (codex P2 round 1 on PR #63): preflight the tray side BEFORE
    // registering the daemon so a tray failure does not leave a
    // half-installed service. Catches: non-Windows host, missing
    // kb-mcp-tray.exe sibling, pre-existing autostart entry without
    // --force. The actual `install_autostart` call below runs only if
    // preflight passed.
    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    let preflight_tray_exe: Option<PathBuf> = if params.with_tray {
        #[cfg(not(target_os = "windows"))]
        {
            return Err(anyhow!("--with-tray is only supported on Windows"));
        }
        #[cfg(target_os = "windows")]
        {
            let bin_dir = ctx
                .binary_path
                .parent()
                .ok_or_else(|| anyhow!("no parent directory for the current kb-mcp.exe"))?
                .to_path_buf();
            let tray_exe = bin_dir.join("kb-mcp-tray.exe");
            kb_mcp_tray::install::preflight_check(&name, &tray_exe, params.force)?;
            Some(tray_exe)
        }
    } else {
        None
    };

    backend().install(&ctx)?;
    eprintln!(
        "Service '{}' installed (config_home: {}).",
        name,
        config_home.display()
    );

    // Tray install runs ONLY if preflight passed above. force=true is
    // safe here because preflight has already validated the duplicate-
    // check rule (= duplicate without --force was rejected before
    // backend().install() ran).
    #[cfg(target_os = "windows")]
    if let Some(tray_exe) = preflight_tray_exe {
        let lnk = kb_mcp_tray::install::install_autostart(&name, &tray_exe, &config_home, true)?;
        eprintln!("Tray autostart shortcut: {}", lnk.display());
    }

    Ok(())
}

#[cfg(target_os = "windows")]
pub fn run_tray_install(service_name: &str, force: bool) -> Result<()> {
    let name = validate_service_name(service_name).map_err(|e| anyhow!(e))?;
    let bin_dir = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow!("no parent directory for the current kb-mcp.exe"))?
        .to_path_buf();
    let tray_exe = bin_dir.join("kb-mcp-tray.exe");
    if !tray_exe.exists() {
        return Err(anyhow!(
            "{} not found. Install kb-mcp-tray.exe from the v0.9.0 release zip into the same directory as kb-mcp.exe.",
            tray_exe.display()
        ));
    }
    let config_home = resolve_config_home(&name)?;
    let lnk = kb_mcp_tray::install::install_autostart(&name, &tray_exe, &config_home, force)?;
    eprintln!("Tray autostart shortcut: {}", lnk.display());
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn run_tray_install(_service_name: &str, _force: bool) -> Result<()> {
    Err(anyhow!("tray-install is only supported on Windows"))
}

fn is_loopback_addr(s: &str) -> bool {
    s.starts_with("127.") || s.starts_with("[::1]") || s.starts_with("localhost")
}

fn write_toml(path: &std::path::Path, kb_path: &std::path::Path, bind: &str) -> Result<()> {
    // Schema must match `kb_mcp::config::Config` (= top-level `kb_path` +
    // `[transport.http]`). `Config` uses `#[serde(deny_unknown_fields)]` so
    // any other section (e.g. `[index]`) would crash `kb-mcp serve` at
    // startup with a parse error.
    //
    // codex P2 round 4 on PR #56: single-quoted TOML literal strings cannot
    // contain `'`, so a path like `/Users/O'Brien/kb` would produce invalid
    // TOML. Use `toml::Value::String(...).to_string()` which emits a proper
    // basic-quoted string with backslash escaping for all path characters
    // (including `\U` on Windows, `'`, etc).
    let kb_lit = toml::Value::String(kb_path.display().to_string()).to_string();
    let bind_lit = toml::Value::String(bind.to_string()).to_string();
    let content = format!("kb_path = {kb_lit}\n\n[transport.http]\nbind = {bind_lit}\n");
    std::fs::write(path, content)?;
    Ok(())
}

/// kb_path を解決 (spec § Q1 c-3 hybrid):
/// 1. `--kb-path` flag (= Some(flag)) が指定されたらそれ
/// 2. それ以外で toml_path が指定されたら toml の top-level `kb_path` を読む
///    (= `kb_mcp::config::Config` schema と同じ key、`[index]` は存在しない)
/// 3. 両方 None なら error
pub fn resolve_kb_path(flag: Option<PathBuf>, toml_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = flag {
        return Ok(p);
    }
    let Some(toml_path) = toml_path else {
        return Err(anyhow!(
            "kb_path が解決できません: --kb-path flag を指定するか、kb-mcp.toml に top-level `kb_path` を書いてください"
        ));
    };
    let content = std::fs::read_to_string(&toml_path)
        .with_context(|| format!("kb-mcp.toml 読込失敗: {}", toml_path.display()))?;
    let parsed: toml::Value = toml::from_str(&content)?;
    let kb_path = parsed
        .get("kb_path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow!(
                "{} に top-level `kb_path` がありません",
                toml_path.display()
            )
        })?;
    Ok(PathBuf::from(kb_path))
}
