//! `kb-mcp --config <path>` global flag の e2e。tests/validate_cli.rs の TempKb
//! パターンを踏襲。embedding DL 不要なので通常の `cargo test` に載せる。

use std::path::{Path, PathBuf};
use std::process::Command;

fn kb_mcp_bin() -> Option<PathBuf> {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    #[cfg(windows)]
    let bin = target.join(profile).join("kb-mcp.exe");
    #[cfg(not(windows))]
    let bin = target.join(profile).join("kb-mcp");
    if bin.exists() { Some(bin) } else { None }
}

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new(prefix: &str) -> Self {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("{prefix}-{pid}-{nonce}"));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
    #[allow(dead_code)]
    fn path(&self) -> &Path {
        &self.path
    }
    #[allow(dead_code)]
    fn write(&self, rel: &str, content: &str) {
        let full = self.path.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

impl TempDir {
    fn run_kb_mcp(&self, args: &[&str]) -> std::process::Output {
        let bin = kb_mcp_bin().expect("kb-mcp binary must be built");
        let mut cmd = Command::new(bin);
        cmd.current_dir(&self.path);
        cmd.args(args);
        cmd.output().expect("spawn kb-mcp")
    }
}

fn stderr_str(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// ANSI エスケープシーケンス (`ESC[...m` 等) を除去してプレーンテキストを返す。
/// tracing-subscriber がターミナルを検知して色を付けた場合に備える。
///
/// 対応範囲: CSI シーケンス (`ESC [ <params> <final-byte>`)。tracing-subscriber が
/// 出すのは SGR (`ESC[...m`) のみなのでこれで十分。それ以外の ESC シーケンス
/// (`ESC c` 等の単発系) は ESC ごとプレーンテキストとして出力する pass-through。
/// Test-only ヘルパなので allocation 効率より明瞭さ優先。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            // CSI sequence: ESC [ ... (終端は ASCII alphabetic)
            chars.next(); // consume '['
            for ch in chars.by_ref() {
                if ch.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn test_explicit_config_missing_fails_fast() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let dir = TempDir::new("kb-mcp-disc-explicit-miss");
    let nope = dir.path().join("nope.toml");
    let out = Command::new(&bin)
        .args(["--config"])
        .arg(&nope)
        .args(["status", "--kb-path", "/tmp/whatever"])
        .output()
        .expect("spawn kb-mcp");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit, stderr={stderr}"
    );
    assert!(
        stderr.contains("--config") && stderr.contains("not found"),
        "stderr must mention `--config ... not found`: {stderr}"
    );
}

#[test]
fn test_explicit_config_takes_priority_over_cwd() {
    let Some(_) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let dir = TempDir::new("kb-mcp-disc-prio");
    dir.write("kb-mcp.toml", "kb_path = \"/should-not-win\"\n");
    let explicit = dir.path().join("real.toml");
    let kb = dir.path().join("should-win");
    std::fs::create_dir_all(&kb).unwrap();
    std::fs::write(
        &explicit,
        format!(
            "kb_path = \"{}\"\n",
            kb.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    // status は kb_path 配下に DB が無くても起動して exit 0 になる。stderr に source=Explicit が出る。
    let out = dir.run_kb_mcp(&[
        "--config",
        explicit.to_str().unwrap(),
        "status",
        "--kb-path",
        kb.to_str().unwrap(),
    ]);
    let err = stderr_str(&out);
    let plain = strip_ansi(&err);
    assert!(
        plain.contains("source=Explicit"),
        "stderr must show Explicit: {err}"
    );
}

#[test]
fn test_cwd_picked_when_no_explicit() {
    let Some(_) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let dir = TempDir::new("kb-mcp-disc-cwd");
    let kb = dir.path().join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    dir.write(
        "kb-mcp.toml",
        &format!(
            "kb_path = \"{}\"\n",
            kb.to_string_lossy().replace('\\', "/")
        ),
    );
    let out = dir.run_kb_mcp(&["status"]);
    let err = stderr_str(&out);
    let plain = strip_ansi(&err);
    assert!(plain.contains("source=Cwd"), "stderr must show Cwd: {err}");
}

#[test]
fn test_walks_to_git_root() {
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let dir = TempDir::new("kb-mcp-disc-git");
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    let kb = dir.path().join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    dir.write(
        "kb-mcp.toml",
        &format!(
            "kb_path = \"{}\"\n",
            kb.to_string_lossy().replace('\\', "/")
        ),
    );
    let nested = dir.path().join("a/b/c");
    std::fs::create_dir_all(&nested).unwrap();
    // current_dir = nested、toml は祖先の git root 直下。
    let out = Command::new(bin)
        .current_dir(&nested)
        .args(["status"])
        .output()
        .expect("spawn");
    let err = stderr_str(&out);
    let plain = strip_ansi(&err);
    assert!(
        plain.contains("source=GitRoot"),
        "stderr must show GitRoot: {err}"
    );
}

#[test]
fn test_default_when_no_toml_anywhere() {
    let Some(_) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let dir = TempDir::new("kb-mcp-disc-none");
    // CWD / .git / バイナリ隣 (binary はリポジトリ target 配下なので、そちらに kb-mcp.toml が
    // 偶然あると AlongsideBinary 経路に入ってしまう)。
    // → リポジトリ target 配下の kb-mcp.toml は staging 禁止 (.gitignore 済) なので
    //   通常の dev 環境では存在しないが、念のため status コマンドを kb_path 明示で呼んで
    //   source=NotFound または source=AlongsideBinary のいずれかが出ることだけ確認する。
    let kb = dir.path().join("kb");
    std::fs::create_dir_all(&kb).unwrap();
    let out = dir.run_kb_mcp(&["status", "--kb-path", kb.to_str().unwrap()]);
    let err = stderr_str(&out);
    let plain = strip_ansi(&err);
    assert!(
        plain.contains("source=NotFound") || plain.contains("source=AlongsideBinary"),
        "stderr must show NotFound or AlongsideBinary: {err}"
    );
}

#[test]
fn test_explicit_with_tilde_expands() {
    // `~` 展開は home が取れる環境でのみ意味があるので、HOME / USERPROFILE が
    // 取れない環境は skip。
    let Some(bin) = kb_mcp_bin() else {
        eprintln!("kb-mcp binary not built — skipping");
        return;
    };
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let Some(home) = home else {
        eprintln!("HOME/USERPROFILE not set — skipping");
        return;
    };
    let home = std::path::PathBuf::from(home);
    let stamp = format!(
        "kb-mcp-disc-tilde-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let toml_in_home = home.join(format!("{stamp}.toml"));
    let kb_in_home = home.join(format!("{stamp}-kb"));
    // Drop guard を **先に** 構築。下の `unwrap()` が panic しても、既に作られた
    // ファイル / ディレクトリは scope 終了で確実に消える (Cleanup::drop は
    // remove_file / remove_dir_all を `let _` で握り潰すので、未作成パスでも安全)。
    let _cleanup = scopeguard_like(toml_in_home.clone(), kb_in_home.clone());
    std::fs::create_dir_all(&kb_in_home).unwrap();
    std::fs::write(
        &toml_in_home,
        format!(
            "kb_path = \"{}\"\n",
            kb_in_home.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();

    let tilde_arg = format!("~/{stamp}.toml");
    let out = Command::new(bin)
        .args(["--config", &tilde_arg, "status"])
        .output()
        .expect("spawn");
    let err = stderr_str(&out);
    let plain = strip_ansi(&err);
    assert!(
        plain.contains("source=Explicit"),
        "tilde expansion must resolve and load: {err}"
    );
}

/// HOME に置いたゴミファイルを Drop で消す自前 guard。
/// パスは保存して所有する (テスト関数末尾で確実に消えるよう)。
struct Cleanup {
    file: PathBuf,
    dir: PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn scopeguard_like(file: PathBuf, dir: PathBuf) -> Cleanup {
    Cleanup { file, dir }
}
