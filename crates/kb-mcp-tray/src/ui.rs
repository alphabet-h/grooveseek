use anyhow::{Result, bail};

/// Launch the user's default browser at `ui_url` (= `Config::ui_url`,
/// pre-built in config.rs so we never re-derive it from `base_url`).
///
/// AU-12: これは以前 `cmd /c start "" <url>` だった。`cmd.exe` は `/c` の
/// 後ろを**コマンドラインとして**解釈するため、URL に `&` が入ると
/// そこで区切られて後続がコマンドとして実行される。Rust の
/// `std::process::Command` は空白かタブを含む引数しか quote しないので、
/// `&` を含むだけの引数は素通りする (実測: `cmd /c echo <url>&ver` で
/// `ver` が実行された)。`bind` は `kb-mcp.toml` 由来なので、これは設定
/// ファイルからのコマンド実行経路になっていた。
///
/// `ShellExecuteW` は `lpFile` を **Shell オブジェクト名**として扱い、
/// コマンドラインとして解析しない。したがって shell メタ文字の意味が
/// そもそも存在しない。
///
/// ただし `ShellExecuteW` は「指定されたファイルに対して操作を行う」API
/// でもあるので、実行可能ファイルのパスを渡せばそれを起動してしまう。
/// URL であることを [`is_http_url`] で確かめてから渡す (config.rs の
/// 検証と合わせて二重にする)。
#[cfg(target_os = "windows")]
pub fn open_web_ui(ui_url: &str) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    if !is_http_url(ui_url) {
        bail!("refusing to open {ui_url:?}: only http:// and https:// URLs are opened");
    }

    let file: Vec<u16> = std::ffi::OsStr::new(ui_url)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `file` と `verb` はいずれも NUL 終端の UTF-16 バッファで、
    // 呼び出しの間ずっと生存する。残りの引数は NULL 許容と docs にある。
    //
    // COM は初期化しない: 本関数は tao の event loop (= tray が動く
    // メインスレッド) からのみ呼ばれ、そこは既に GUI 用に初期化済み。
    // ここで `CoInitializeEx` を呼ぶとアパートメント種別が食い違って
    // `RPC_E_CHANGED_MODE` になりうるので触らない。
    let hinstance = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    // 公式仕様: 成功すると 32 より大きい値が返る。32 以下は SE_ERR_* の
    // エラーコード (HINSTANCE 型なのは 16-bit Windows との互換のため)。
    let code = hinstance.0 as usize;
    if code <= 32 {
        bail!("ShellExecuteW failed to open {ui_url:?} (SE_ERR code {code})");
    }
    Ok(())
}

/// `ShellExecuteW` に渡してよいのは http(s) の URL だけ。ファイルパスを
/// 渡すとそれを開く / 実行するため。
///
/// スキームの比較は ASCII 大文字小文字を無視する (URL のスキームは
/// case-insensitive)。
fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_urls_are_accepted() {
        assert!(is_http_url("http://127.0.0.1:3100/ui"));
        assert!(is_http_url("https://127.0.0.1:3100/ui"));
        assert!(is_http_url("HTTP://127.0.0.1:3100/ui"));
    }

    #[test]
    fn anything_that_is_not_an_http_url_is_refused() {
        // ShellExecuteW はファイルパスを渡せばそれを開く / 実行するので、
        // スキームの確認が最後の砦になる。
        for bad in [
            "C:\\Windows\\System32\\calc.exe",
            "\\\\attacker\\share\\payload.exe",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:",
            "javascript:alert(1)",
            "",
            "127.0.0.1:3100/ui",
        ] {
            assert!(!is_http_url(bad), "should have refused {bad:?}");
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn open_web_ui_refuses_a_non_http_target_without_calling_the_shell() {
        let err = open_web_ui("C:\\Windows\\System32\\calc.exe").unwrap_err();
        assert!(
            err.to_string().contains("only http"),
            "unexpected error: {err}"
        );
    }
}
