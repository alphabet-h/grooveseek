//! macOS LaunchAgent backend for groove service.
//!
//! Module-level `#[cfg(target_os = "macos")]` lives on the `pub mod macos;`
//! declaration in `src/service/mod.rs`; no inner `#![cfg]` needed.

use super::render::{LAUNCHD_STDERR_LOG, LAUNCHD_STDOUT_LOG};
use super::{InstallContext, ServiceBackend, ServiceState};
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct LaunchAgent;

/// テンプレート本体は `service::render` にある (全 OS で compile + テストする
/// ため)。ここは既存の呼び出し経路 `service::macos::render_plist` を保つための
/// re-export。
pub use super::render::render_plist;

fn plist_path(service_name: &str) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("HOME 解決失敗"))?;
    Ok(home.join(format!(
        "Library/LaunchAgents/com.groove.{}.plist",
        service_name
    )))
}

fn current_uid() -> Result<String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .context("id -u 実行失敗")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_launchctl(args: &[&str]) -> Result<()> {
    let status = Command::new("launchctl")
        .args(args)
        .status()
        .with_context(|| format!("launchctl {} 実行失敗", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "launchctl {} が失敗 (status: {})",
            args.join(" "),
            status
        ));
    }
    Ok(())
}

/// 既に存在する LaunchAgent の log を 0600 に締め直す (BU-24)。
///
/// plist の `Umask` は launchd が log file を**作る瞬間**にしか効かないので、
/// キーが無かった頃に作られた 0644 の file は `service install --force` で
/// 上書きしても mode が残る。plist を書いた後にここで拾う。
///
/// file が無いのが通常 (新規インストール、または `--no-auto-start`)。chmod に
/// 失敗しても install 自体は成立しているので、warn に留めて中断しない。
fn tighten_existing_logs(config_home: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for name in [LAUNCHD_STDOUT_LOG, LAUNCHD_STDERR_LOG] {
        let path = config_home.join(name);
        if !path.exists() {
            continue;
        }
        if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
            eprintln!(
                "warning: 既存ログの permission を 0600 にできませんでした: {} ({e})",
                path.display()
            );
        }
    }
}

impl ServiceBackend for LaunchAgent {
    fn install(&self, ctx: &InstallContext) -> Result<()> {
        let path = plist_path(&ctx.service_name)?;
        // `plist_path` は必ず `~/Library/LaunchAgents/` 配下を返すので親は
        // 存在するはずだが、`unwrap()` はその「はず」を実行時 panic で表明する。
        // 呼び出し元は `Result` を扱えるので、そちらに載せる。
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("plist パスに親ディレクトリが無い: {}", path.display()))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("LaunchAgents ディレクトリ作成失敗: {}", parent.display()))?;
        if path.exists() && !ctx.force {
            return Err(anyhow!(
                "plist が既存: {} (--force で上書き)",
                path.display()
            ));
        }
        std::fs::write(&path, render_plist(ctx))?;
        tighten_existing_logs(&ctx.config_home);
        if ctx.auto_start {
            let uid = current_uid()?;
            // (codex P2 round 2 on PR #156) `bootstrap` does **not** replace an
            // already-loaded job — it fails. So `service install --force` over a
            // running agent used to error out *and* leave launchd holding the
            // old in-memory `ProgramArguments`, which is exactly the upgrade
            // path that has to work now that the launch line carries `--config`.
            // Unload first, best-effort: on a fresh install there is nothing to
            // unload and `bootout` exits non-zero, which is not a failure.
            // `uninstall` already treats it the same way.
            let _ = run_launchctl(&[
                "bootout",
                &format!("gui/{}/com.groove.{}", uid, ctx.service_name),
            ]);
            // `launchctl` は引数を `&str` で受けるので、UTF-8 でないホーム
            // ディレクトリではここが唯一の失敗点になる。panic ではなく
            // 「なぜ install できないか」として返す。
            let path_arg = path
                .to_str()
                .ok_or_else(|| anyhow!("plist パスが UTF-8 でない: {}", path.display()))?;
            run_launchctl(&["bootstrap", &format!("gui/{}", uid), path_arg])?;
        }
        Ok(())
    }
    fn uninstall(&self, service_name: &str) -> Result<()> {
        let path = plist_path(service_name)?;
        if path.exists() {
            let uid = current_uid().unwrap_or_default();
            let _ = run_launchctl(&[
                "bootout",
                &format!("gui/{}/com.groove.{}", uid, service_name),
            ]);
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }
    fn status(&self, service_name: &str) -> Result<ServiceState> {
        let out = Command::new("launchctl")
            .args(["list", &format!("com.groove.{}", service_name)])
            .output()
            .context("launchctl list 実行失敗")?;
        if !out.status.success() {
            // exit 1 from launchctl list <label> = label not loaded
            return Ok(ServiceState::NotFound);
        }
        // codex P2 round 4 on PR #56: `launchctl list <label>` exits 0 for
        // loaded-but-not-running agents (= LastExitStatus != 0, no PID).
        // Distinguish Running vs Stopped by checking for a numeric PID line
        // in the plist-style output (`"PID" = NNN;`).
        let stdout = String::from_utf8_lossy(&out.stdout);
        let has_pid = stdout
            .lines()
            .map(str::trim)
            .any(|l| l.starts_with("\"PID\" =") || l.starts_with("PID ="));
        Ok(if has_pid {
            ServiceState::Running {
                uptime_secs: 0,
                bind: None,
                kb_path: None,
                model: None,
            }
        } else {
            ServiceState::Stopped {
                bind: None,
                kb_path: None,
            }
        })
    }
    fn list(&self) -> Result<Vec<(String, ServiceState)>> {
        let dir = dirs::home_dir()
            .ok_or_else(|| anyhow!("HOME 解決失敗"))?
            .join("Library/LaunchAgents");
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(rest) = name
                .strip_prefix("com.groove.")
                .and_then(|s| s.strip_suffix(".plist"))
            {
                let state = self.status(rest)?;
                out.push((rest.to_string(), state));
            }
        }
        Ok(out)
    }
    fn stop(&self, service_name: &str) -> Result<()> {
        let uid = current_uid()?;
        run_launchctl(&[
            "bootout",
            &format!("gui/{}/com.groove.{}", uid, service_name),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct DirGuard(PathBuf);
    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The plist's `Umask` only applies when launchd *creates* the log, so an
    /// agent installed before BU-24 keeps its 0644 files across an upgrade.
    #[test]
    fn reinstalling_tightens_logs_left_world_readable_by_an_older_agent() {
        let dir = crate::test_support::unique_temp_path("groove-macos-logs");
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = DirGuard(dir.clone());

        let out = dir.join(LAUNCHD_STDOUT_LOG);
        let err = dir.join(LAUNCHD_STDERR_LOG);
        for path in [&out, &err] {
            std::fs::write(path, b"stale log\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        tighten_existing_logs(&dir);

        for path in [&out, &err] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode,
                0o600,
                "{} stayed at {mode:o} — an upgrade must not leave the daemon's \
                 stderr readable by other accounts",
                path.display()
            );
        }
    }

    /// The usual case: a fresh install, or `--no-auto-start`, where launchd has
    /// not created anything yet. Absent files must not fail the install.
    #[test]
    fn tightening_is_a_no_op_when_the_logs_do_not_exist_yet() {
        let dir = crate::test_support::unique_temp_path("groove-macos-nologs");
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = DirGuard(dir.clone());

        tighten_existing_logs(&dir);

        assert!(!dir.join(LAUNCHD_STDOUT_LOG).exists());
        assert!(!dir.join(LAUNCHD_STDERR_LOG).exists());
    }
}
