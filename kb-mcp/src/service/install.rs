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
    // (v0.9.2 hot-fix) When `path` already exists (= `install --force` re-
    // running over a user-customized toml), parse with `toml_edit` and
    // overwrite ONLY `kb_path` and `[transport.http].bind`. All other
    // user-set fields (`model`, `fastembed_cache_dir`, `exclude_dirs`,
    // `[best_practice]`, etc.), inline comments, and field ordering are
    // preserved verbatim. Without this, v0.9.0 / v0.9.1 dogfood revealed
    // that `--force` replaced a 1.6 KB user config with a 5-line minimal
    // toml — making the daemon crash with `embedding model mismatch`
    // when the DB had been indexed with `bge-m3` (1024 dim) but the
    // regenerated toml fell back to the default `bge-small` (384 dim).
    //
    // (codex P2 round 4 on PR #56) Single-quoted TOML literal strings
    // cannot contain `'`, so a path like `/Users/O'Brien/kb` would produce
    // invalid TOML. `toml_edit::value()` emits a basic-quoted string with
    // backslash escaping (same as the legacy `toml::Value` path).
    use toml_edit::{DocumentMut, Item, Table, value};

    let mut doc: DocumentMut = if path.exists() {
        let existing = std::fs::read_to_string(path)
            .with_context(|| format!("kb-mcp.toml 読込失敗: {}", path.display()))?;
        existing.parse::<DocumentMut>().with_context(|| {
            format!(
                "kb-mcp.toml が invalid TOML です: {}。手動で修正してから再 install してください (--force でも auto-overwrite しません)",
                path.display()
            )
        })?
    } else {
        DocumentMut::new()
    };

    doc["kb_path"] = value(kb_path.display().to_string());

    // (codex-review P2 round 1 on PR #65) The naive
    // `doc["transport"]["http"]["bind"] = value(...)` form panics at runtime
    // when an existing toml has `transport` (or `transport.http`) as a non-
    // table item (= scalar, array). Hand-edited configs occasionally hit
    // this; surface a descriptive error pointing at the path instead of
    // exploding. Also force `set_implicit(true/false)` so a fresh install
    // produces the canonical `[transport.http]` block form instead of the
    // dotted-key style that `IndexMut` auto-creates by default.
    let root = doc.as_table_mut();
    let transport_item = root.entry("transport").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true); // [transport] header is unused; [transport.http] is the canonical block.
        Item::Table(t)
    });
    let transport = transport_item.as_table_mut().ok_or_else(|| {
        anyhow!(
            "kb-mcp.toml の `transport` キーが table ではありません: {}。手動で修正してから再 install してください",
            path.display()
        )
    })?;
    let http_item = transport.entry("http").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(false); // emit `[transport.http]` header verbatim on fresh installs.
        Item::Table(t)
    });
    let http = http_item.as_table_mut().ok_or_else(|| {
        anyhow!(
            "kb-mcp.toml の `[transport.http]` セクションが table ではありません: {}。手動で修正してから再 install してください",
            path.display()
        )
    })?;
    http["bind"] = value(bind.to_string());

    std::fs::write(path, doc.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    // CLAUDE.local.md: do NOT use `tempfile` crate. Build a unique path under
    // `std::env::temp_dir()` from pid + nanos + counter, and clean up via Drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("kb-mcp-write-toml-test-{tag}-{pid}-{nanos}-{seq}"));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn write_toml_creates_minimal_doc_when_file_absent() {
        let tmp = TempDir::new("new");
        let toml_path = tmp.path().join("kb-mcp.toml");
        let kb = PathBuf::from("/tmp/kb");
        write_toml(&toml_path, &kb, "127.0.0.1:3100").unwrap();

        let body = std::fs::read_to_string(&toml_path).unwrap();
        let parsed: toml_edit::DocumentMut = body.parse().unwrap();
        assert_eq!(parsed["kb_path"].as_str().unwrap(), "/tmp/kb");
        assert_eq!(
            parsed["transport"]["http"]["bind"].as_str().unwrap(),
            "127.0.0.1:3100"
        );
        // (P2 round 1 on PR #65) Lock down the canonical `[transport.http]`
        // header form for fresh installs. Without `set_implicit(false)` on
        // the new table, toml_edit emits dotted-key syntax
        // (`transport.http.bind = ...`) which parses identically but is
        // unfamiliar to users reading the example files.
        assert!(
            body.contains("[transport.http]"),
            "fresh install should emit explicit [transport.http] header, got:\n{body}"
        );
    }

    #[test]
    fn write_toml_errors_when_existing_transport_is_scalar_not_table() {
        // (P2 round 1 on PR #65) Defense against a panic when a hand-edited
        // toml has `transport = "something"` (= scalar) at top level: the
        // naive `doc["transport"]["http"]` IndexMut would panic. The merge
        // path must instead surface a descriptive error so the user can
        // fix the file by hand.
        let tmp = TempDir::new("scalar");
        let toml_path = tmp.path().join("kb-mcp.toml");
        std::fs::write(&toml_path, "kb_path = \"/old\"\ntransport = \"stdio\"\n").unwrap();

        let result = write_toml(&toml_path, &PathBuf::from("/new"), "127.0.0.1:3100");
        assert!(
            result.is_err(),
            "expected error when `transport` is a scalar"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("`transport`") && err.contains("table"),
            "error should explain that `transport` is not a table, got: {err}"
        );
        // File must remain untouched (= no partial overwrite).
        let body = std::fs::read_to_string(&toml_path).unwrap();
        assert!(body.contains("transport = \"stdio\""));
    }

    #[test]
    fn write_toml_preserves_user_customized_fields_on_force_rewrite() {
        // Regression test for the v0.9.0 dogfood finding: `install --force`
        // used to obliterate `model` / `fastembed_cache_dir` / `exclude_dirs`
        // and crash the daemon with `embedding model mismatch`. The merge
        // logic must keep every key that isn't `kb_path` or
        // `[transport.http].bind`.
        let tmp = TempDir::new("preserve");
        let toml_path = tmp.path().join("kb-mcp.toml");
        let original = concat!(
            "kb_path = \"/old/path\"\n",
            "model = \"bge-m3\"\n",
            "fastembed_cache_dir = \"/cache/hf\"\n",
            "exclude_dirs = [\".obsidian\", \"weeknotes\"]\n",
            "\n",
            "[best_practice]\n",
            "path_templates = [\"best-practices/{target}/PERFECT.md\"]\n",
            "\n",
            "[transport.http]\n",
            "bind = \"127.0.0.1:3000\"\n",
        );
        std::fs::write(&toml_path, original).unwrap();

        write_toml(&toml_path, &PathBuf::from("/new/path"), "127.0.0.1:3100").unwrap();

        let body = std::fs::read_to_string(&toml_path).unwrap();
        let doc: toml_edit::DocumentMut = body.parse().unwrap();
        // CLI-managed fields overwritten:
        assert_eq!(doc["kb_path"].as_str().unwrap(), "/new/path");
        assert_eq!(
            doc["transport"]["http"]["bind"].as_str().unwrap(),
            "127.0.0.1:3100"
        );
        // User-customized fields preserved:
        assert_eq!(doc["model"].as_str().unwrap(), "bge-m3");
        assert_eq!(doc["fastembed_cache_dir"].as_str().unwrap(), "/cache/hf");
        let exclude_dirs = doc["exclude_dirs"].as_array().unwrap();
        assert_eq!(exclude_dirs.len(), 2);
        assert_eq!(exclude_dirs.get(0).unwrap().as_str().unwrap(), ".obsidian");
        assert_eq!(exclude_dirs.get(1).unwrap().as_str().unwrap(), "weeknotes");
        let path_templates = doc["best_practice"]["path_templates"].as_array().unwrap();
        assert_eq!(path_templates.len(), 1);
        assert_eq!(
            path_templates.get(0).unwrap().as_str().unwrap(),
            "best-practices/{target}/PERFECT.md"
        );
    }

    #[test]
    fn write_toml_preserves_inline_comments_on_force_rewrite() {
        // toml_edit (= unlike `toml`/serde) round-trips comments. Verify
        // explicitly so we catch a regression if the dep gets swapped back.
        let tmp = TempDir::new("comments");
        let toml_path = tmp.path().join("kb-mcp.toml");
        let original = concat!(
            "# top-level comment about kb_path\n",
            "kb_path = \"/old/path\"\n",
            "# inline reasoning for model choice\n",
            "model = \"bge-m3\"\n",
            "\n",
            "[transport.http]\n",
            "# bind is loopback-only by default\n",
            "bind = \"127.0.0.1:3100\"\n",
        );
        std::fs::write(&toml_path, original).unwrap();

        write_toml(&toml_path, &PathBuf::from("/new/path"), "127.0.0.1:3200").unwrap();

        let body = std::fs::read_to_string(&toml_path).unwrap();
        assert!(
            body.contains("# top-level comment about kb_path"),
            "lost top-level comment:\n{body}"
        );
        assert!(
            body.contains("# inline reasoning for model choice"),
            "lost model comment:\n{body}"
        );
        assert!(
            body.contains("# bind is loopback-only by default"),
            "lost bind comment:\n{body}"
        );
    }

    #[test]
    fn write_toml_errors_on_invalid_toml_instead_of_overwriting() {
        // If the existing file is unparseable, refuse to overwrite — the
        // user might have an in-progress hand-edit. Surface a clear error
        // pointing at the path so they can fix it manually.
        let tmp = TempDir::new("invalid");
        let toml_path = tmp.path().join("kb-mcp.toml");
        std::fs::write(&toml_path, "this = is = not = valid = toml").unwrap();

        let result = write_toml(&toml_path, &PathBuf::from("/new/path"), "127.0.0.1:3100");
        assert!(result.is_err(), "expected error on invalid TOML");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("invalid TOML"),
            "error should mention invalid TOML, got: {err}"
        );
        // Existing (invalid) file must not have been overwritten.
        let body = std::fs::read_to_string(&toml_path).unwrap();
        assert_eq!(body, "this = is = not = valid = toml");
    }
}
