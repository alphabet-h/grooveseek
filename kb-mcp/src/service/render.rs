//! Unit / plist のテンプレート生成 (OS 非依存)。
//!
//! `linux.rs` / `macos.rs` は `#[cfg(target_os = ...)]` で片方しか compile
//! されないため、そこにテンプレートを置くと **plist の誤りは macOS runner
//! でしか、unit の誤りは Linux runner でしか検出されない**。中身は
//! `InstallContext` から文字列を組むだけの純粋関数なので、ここに置いて
//! 全 OS で compile + テストする (AU-07/08 で `child_args` を全 OS compile に
//! したのと同じ理由)。
//!
//! `linux::render_unit` / `macos::render_plist` は本 module への re-export。

use super::InstallContext;
use anyhow::Result;

/// LaunchAgent の stdout / stderr の落ち先 (config home からの相対名)。
///
/// plist を書く側 (本 module) と、既存インストールの log を締め直す側
/// (`macos::tighten_existing_logs`) の**両方**が使う。片方だけ名前を変えると
/// chmod が空振りするので、定数を 1 つにして drift を型で防ぐ。
pub(crate) const LAUNCHD_STDOUT_LOG: &str = "kb-mcp.out";
pub(crate) const LAUNCHD_STDERR_LOG: &str = "kb-mcp.err";

/// `service install` が config home に書く設定ファイル名。
///
/// 書く側 (`install::run_with_backend`) と、それを `--config` で名指しする
/// 3 つの launch line が同じ定数を見る。片方だけ変えると daemon が
/// **起動時に「そんなファイルは無い」で落ちる**ので、drift を型で防ぐ
/// ([`LAUNCHD_STDOUT_LOG`] と同じ理由)。
pub(crate) const SERVICE_CONFIG_FILE: &str = "kb-mcp.toml";

/// launch line が名指しする config ファイルの絶対パス。
///
/// **なぜ相対パスにしないのか** (BU-07 の残穴): 3 backend とも
/// WorkingDirectory を config home にしているので `--config kb-mcp.toml` でも
/// 届くし、quoting の問題も丸ごと消える。それでも絶対パスにするのは、
/// 相対形の失敗モードが**信頼側に倒れる**ため — WorkingDirectory が効かない
/// 環境では、CWD にたまたま在る `kb-mcp.toml` を `ConfigSource::Explicit` =
/// Trusted として読んでしまう。絶対パスの失敗モードは「ファイルが無い」で、
/// 起動しないだけで済む。可用性の失敗は診断できるが、信頼の失敗はできない。
///
/// **`Path::join` を使わず `/` で繋ぐ**理由: unit と plist は Linux / macOS
/// 専用の成果物だが、このモジュールは全 OS で compile + テストされる (それが
/// このモジュールの存在理由)。`join` を使うと Windows ホストで走らせた時だけ
/// 区切りが `\` になり、テストの期待値が host 依存になる。plist の
/// `StandardOutPath` が既に同じ理由で `{home}/{out_log}` と書いている。
/// Windows の launch line は `windows::build_register_script` が native な
/// `join` で組む。
fn posix_config_path(config_home: &str) -> String {
    format!("{config_home}/{SERVICE_CONFIG_FILE}")
}

/// XML の element content として安全になるようエスケープする。
///
/// [XML 1.0 §2.4](https://www.w3.org/TR/xml/#syntax) より:
///
/// > The ampersand character (&) and the left angle bracket (<) MUST NOT appear
/// > in their literal form ... The right angle bracket (>) ... MUST, for
/// > compatibility, be escaped ... when it appears in the string "]]>".
///
/// `>` は `]]>` の中でのみ必須だが、文脈判定を持ち込まずに常にエスケープする。
pub(crate) fn escape_xml_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// systemd unit の値として表現できない文字が無いか検査する。
///
/// 対象は改行 (`\n` / `\r`) と NUL。unit file は行指向なので、値に改行が
/// 入るとそこから先が **別のディレクティブとして解釈される**。Linux の
/// ファイル名としては合法なので、`current_exe()` 由来で実際に起こりうる。
///
/// エスケープではなく reject にしているのは、埋め込み先
/// (`WorkingDirectory=`) が quote を解釈するのかを確認できていないため。
/// systemd.syntax(7) は quoting 規則を「For settings **where quoting is
/// allowed**」と条件付きで書くだけで、対象設定を列挙していない。解釈されない
/// 設定に quote を出すと **今動いているパスまで壊す**ので、install 時に理由を
/// 添えて失敗する方を選ぶ (次回ログインで unit が読めず黙って起動しない、より良い)。
pub(crate) fn ensure_unit_value_representable(label: &str, value: &str) -> Result<()> {
    if let Some(ch) = value.chars().find(|c| matches!(c, '\n' | '\r' | '\0')) {
        let what = match ch {
            '\n' => "a newline",
            '\r' => "a carriage return",
            _ => "a NUL byte",
        };
        anyhow::bail!(
            "{label} contains {what}, which a systemd unit file cannot represent: {value:?}"
        );
    }
    Ok(())
}

/// `ExecStart=` の 1 語として安全な形にする。
///
/// systemd.service(5) は ExecStart について
/// 「Each command line is unquoted using the rules described in "Quoting"
/// section in systemd.syntax(7)」と明記しており、**quote が確実に効く**数少ない
/// 設定。空白を含むパスを素で書くと最初の空白で語が切れ、`/home/a b/kb-mcp` が
/// コマンド `/home/a` + 引数 `b/kb-mcp` になる。
///
/// **`%` と `$` は quote では守れない。** 理由は別々:
///
/// - specifier 展開は unquote より前に走るので、リテラルの `%` は `%%` と書く
///   (systemd.unit(5) SPECIFIERS)
/// - 環境変数展開は command line に対して常に効き、systemd.service(5) COMMAND
///   LINES は「to pass a literal dollar sign, use `$$`」と明記している。
///   同節の「quotes are respected when splitting into words, and afterwards
///   removed」が示すとおり **quote は展開を止めない** ので、`"..."` の中でも
///   `$$` が要る。`/srv/${TENANT}/cfg` のような config home は
///   `KB_MCP_CONFIG_HOME` に実際に書ける値で、放置すると daemon が
///   **installer が書いたのとは別のパス**を読む (codex P2 round 1 on PR #156)。
///
/// quote が要らない値は**素のまま返す**。生成される unit を読みやすく保ち、
/// 既存の出力を変えないため。
pub(crate) fn systemd_exec_word(raw: &str) -> String {
    // 順序は無関係 (`%%` は `$` を、`$$` は `%` を生まない)。
    let expanded = raw.replace('%', "%%").replace('$', "$$");
    let needs_quotes = expanded
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'));
    if !needs_quotes {
        return expanded;
    }
    let mut out = String::with_capacity(expanded.len() + 2);
    out.push('"');
    for ch in expanded.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// systemd user unit を組み立てる。
///
/// パスに改行が含まれる場合は **unit を書かずに失敗する** (AU-10)。
/// `ExecStart=` に入るバイナリパスと config パスは quote と `%%` / `$$` で保護する。
/// `WorkingDirectory=` を同じように扱わない理由は
/// [`ensure_unit_value_representable`] の doc を参照。
///
/// `--config` を付ける理由は [`posix_config_path`] の doc を参照 (BU-07)。
pub fn render_unit(ctx: &InstallContext) -> Result<String> {
    let home = ctx.config_home.display().to_string();
    let bin = ctx.binary_path.display().to_string();
    let cfg = posix_config_path(&home);
    ensure_unit_value_representable("config_home", &home)?;
    ensure_unit_value_representable("binary_path", &bin)?;
    ensure_unit_value_representable("service_name", &ctx.service_name)?;
    Ok(format!(
        "[Unit]\n\
         Description=kb-mcp loopback HTTP MCP server ({name})\n\
         After=default.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={home}\n\
         ExecStart={exec} serve --config {cfg}\n\
         Restart=on-failure\n\
         RestartSec=5s\n\
         Environment=RUST_LOG=info\n\
         StandardOutput=journal\n\
         StandardError=journal\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        name = ctx.service_name,
        home = home,
        exec = systemd_exec_word(&bin),
        cfg = systemd_exec_word(&cfg),
    ))
}

/// macOS LaunchAgent の plist を組み立てる。
///
/// 埋め込む値は必ず [`escape_xml_text`] を通す (AU-10)。`&` `<` `>` は
/// **macOS のファイル名として合法**で、`/Users/a&b/kb` のようなパスを
/// `<string>` に素で入れると plist が XML として壊れ、`launchctl load` が
/// 読めなくなる。`service_name` は `[a-zA-Z0-9_-]+` に検証済みなので実質
/// no-op だが、埋め込み口を 1 つでも素通しにしない方針で揃えている。
///
/// `Umask` は log の mode のために要る (BU-24)。log file を作るのは
/// **kb-mcp ではなく launchd** で、launchd.plist(5) は `StandardOutPath` /
/// `StandardErrorPath` について「存在しなければ *`Umask` キーで指定された
/// umask(2) を反映した permission で*作成する」と定めている。キーが無いと
/// user domain の既定 umask 022 が効いて **0644 = world-readable** になる。
/// プロセス側で `umask()` を呼んでも遅い — launchd は exec の**前**に開く。
///
/// **値は `<string>` で書く**。同じ man page が「integer を渡す場合、property
/// list は 8 進を表現できないので **10 進**でなければならない。string を渡した
/// 場合は strtoul(3) の規則で整数に変換され、先頭に `0` を置けば 8 進を指定
/// できる」と述べている。`<integer>0077</integer>` は 0o77 ではなく 77 (= 0o115)
/// と解釈されるので、意図が字面に出る `<string>0077</string>` を使う。
///
/// umask は **job 全体**に効くので、daemon が作る `.kb-mcp.db` や WAL も 0600
/// になる。単一ユーザの LaunchAgent なので望ましい方向の変化だが、log だけの
/// 話ではない点は意識しておくこと。
pub fn render_plist(ctx: &InstallContext) -> String {
    // codex P2 round 5 on PR #56: honor `--no-auto-start` by emitting
    // `<false/>` for `RunAtLoad` and `KeepAlive` when auto_start is false.
    // Otherwise launchd would still start (and keep alive) the agent at the
    // next login as soon as it's loaded — `--no-auto-start` becomes a no-op
    // for the LaunchAgent backend.
    let bool_val = if ctx.auto_start {
        "<true/>"
    } else {
        "<false/>"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.kb-mcp.{name}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>serve</string>
        <string>--config</string>
        <string>{config_file}</string>
    </array>
    <key>WorkingDirectory</key>
    <string>{home}</string>
    <key>RunAtLoad</key>
    {bool_val}
    <key>KeepAlive</key>
    {bool_val}
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
    <key>Umask</key>
    <string>0077</string>
    <key>StandardOutPath</key>
    <string>{home}/{out_log}</string>
    <key>StandardErrorPath</key>
    <string>{home}/{err_log}</string>
</dict>
</plist>
"#,
        name = escape_xml_text(&ctx.service_name),
        bin = escape_xml_text(&ctx.binary_path.display().to_string()),
        config_file = escape_xml_text(&posix_config_path(&ctx.config_home.display().to_string())),
        home = escape_xml_text(&ctx.config_home.display().to_string()),
        out_log = LAUNCHD_STDOUT_LOG,
        err_log = LAUNCHD_STDERR_LOG,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx_with(binary: &str, home: &str) -> InstallContext {
        InstallContext {
            service_name: "kb-mcp".into(),
            kb_path: PathBuf::from("/home/u/kb"),
            bind: "127.0.0.1:3100".into(),
            config_home: PathBuf::from(home),
            binary_path: PathBuf::from(binary),
            auto_start: true,
            force: false,
        }
    }

    // ---- escape_xml_text ----

    #[test]
    fn xml_escaping_covers_the_three_markup_characters() {
        assert_eq!(
            escape_xml_text("/Users/a&b/kb"),
            "/Users/a&amp;b/kb",
            "an ampersand is legal in a macOS path and must not reach the XML raw"
        );
        assert_eq!(escape_xml_text("/Users/<x>/kb"), "/Users/&lt;x&gt;/kb");
        assert_eq!(escape_xml_text("a]]>b"), "a]]&gt;b");
    }

    #[test]
    fn xml_escaping_leaves_ordinary_paths_byte_for_byte() {
        // quote / apostrophe は element content では特別ではないので通す。
        for s in [
            "/Users/me/.cargo/bin/kb-mcp",
            "/Users/me/Library/Application Support/kb-mcp/kb-mcp",
            "kb-mcp",
            "it's \"fine\" here",
            "日本語/パス",
        ] {
            assert_eq!(escape_xml_text(s), s, "unexpectedly rewrote {s:?}");
        }
    }

    // ---- ensure_unit_value_representable ----

    #[test]
    fn a_newline_in_a_path_is_refused_rather_than_written_out() {
        let err = ensure_unit_value_representable("binary_path", "/home/u/a\nExecStart=/bin/sh")
            .expect_err("a newline must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("binary_path"),
            "message must name the field: {msg}"
        );
        assert!(
            msg.contains("newline"),
            "message must say what is wrong: {msg}"
        );
    }

    #[test]
    fn carriage_returns_and_nul_are_refused_too() {
        assert!(ensure_unit_value_representable("p", "/home/u/a\rb").is_err());
        assert!(ensure_unit_value_representable("p", "/home/u/a\0b").is_err());
    }

    #[test]
    fn ordinary_paths_pass_the_unit_value_check() {
        for s in [
            "/home/u/.config/kb-mcp/kb-mcp",
            "/home/u/.cargo/bin/kb-mcp",
            "/home/john doe/kb",
            "/srv/100%/kb",
            "/home/u/it's here",
        ] {
            assert!(
                ensure_unit_value_representable("p", s).is_ok(),
                "refused a legal path: {s:?}"
            );
        }
    }

    // ---- systemd_exec_word ----

    #[test]
    fn an_exec_word_without_special_characters_is_left_bare() {
        assert_eq!(
            systemd_exec_word("/home/u/.cargo/bin/kb-mcp"),
            "/home/u/.cargo/bin/kb-mcp"
        );
    }

    #[test]
    fn a_space_in_the_binary_path_gets_quoted() {
        assert_eq!(
            systemd_exec_word("/home/john doe/bin/kb-mcp"),
            "\"/home/john doe/bin/kb-mcp\""
        );
    }

    #[test]
    fn quotes_and_backslashes_are_escaped_inside_the_quoted_form() {
        assert_eq!(systemd_exec_word("/home/a\"b/kb"), "\"/home/a\\\"b/kb\"");
        assert_eq!(systemd_exec_word("/home/a\\b/kb"), "\"/home/a\\\\b/kb\"");
        assert_eq!(systemd_exec_word("/home/a'b/kb"), "\"/home/a'b/kb\"");
    }

    /// systemd expands `${FOO}` and `$FOO` inside `ExecStart` **including
    /// within quotes**, so a literal `$` has to be written `$$`
    /// (systemd.service(5) COMMAND LINES). `KB_MCP_CONFIG_HOME='/srv/${TENANT}'`
    /// is a value someone can really set, and without this the daemon would be
    /// pointed at a path the installer never wrote to.
    #[test]
    fn a_dollar_sign_is_doubled_so_systemd_does_not_expand_it() {
        assert_eq!(
            systemd_exec_word("/srv/${TENANT}/kb-mcp"),
            "/srv/$${TENANT}/kb-mcp"
        );
        assert_eq!(systemd_exec_word("/srv/$HOME/kb-mcp"), "/srv/$$HOME/kb-mcp");
        // Quoting does not stop the expansion, so the doubling has to survive
        // into the quoted form too.
        assert_eq!(
            systemd_exec_word("/srv/${A B}/kb-mcp"),
            "\"/srv/$${A B}/kb-mcp\""
        );
        // Both escapes at once, since a path may carry both.
        assert_eq!(systemd_exec_word("/srv/100%/$X"), "/srv/100%%/$$X");
    }

    /// The config argument goes through the same word treatment as the binary,
    /// so the escaping above has to reach it.
    #[test]
    fn the_config_argument_is_dollar_escaped_in_the_unit() {
        let unit = render_unit(&ctx_with("/opt/kb-mcp", "/srv/${TENANT}/cfg")).unwrap();
        assert!(
            unit.contains("--config /srv/$${TENANT}/cfg/kb-mcp.toml"),
            "an unescaped `$` makes systemd read a different file than the \
             installer wrote: {unit}"
        );
    }

    #[test]
    fn a_percent_is_doubled_whether_or_not_the_word_is_quoted() {
        // specifier 展開は unquote より前に走るので、quote では守れない。
        assert_eq!(systemd_exec_word("/srv/100%/kb"), "/srv/100%%/kb");
        assert_eq!(
            systemd_exec_word("/srv/100% off/kb"),
            "\"/srv/100%% off/kb\""
        );
        assert_eq!(systemd_exec_word("/srv/%h/kb"), "/srv/%%h/kb");
    }

    // ---- render_unit ----

    #[test]
    fn a_plain_unit_is_rendered_unquoted() {
        let unit = render_unit(&ctx_with(
            "/home/u/.cargo/bin/kb-mcp",
            "/home/u/.config/kb-mcp/kb-mcp",
        ))
        .unwrap();
        assert!(unit.contains("ExecStart=/home/u/.cargo/bin/kb-mcp serve"));
        assert!(unit.contains("WorkingDirectory=/home/u/.config/kb-mcp/kb-mcp"));
        assert!(unit.contains("Description=kb-mcp loopback HTTP MCP server (kb-mcp)"));
    }

    #[test]
    fn a_newline_in_a_path_stops_the_unit_from_being_rendered() {
        // 書き出してしまうと、改行より後ろが独立したディレクティブになる。
        let err = render_unit(&ctx_with(
            "/home/u/x\nExecStart=/bin/sh -c evil",
            "/home/u/.config/kb-mcp/kb-mcp",
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("binary_path"),
            "error should name the offending field: {err}"
        );

        let err = render_unit(&ctx_with(
            "/home/u/.cargo/bin/kb-mcp",
            "/home/u/x\nExecStart=/bin/sh -c evil",
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains("config_home"),
            "error should name the offending field: {err}"
        );
    }

    #[test]
    fn a_binary_path_with_spaces_is_quoted_but_the_working_directory_is_not() {
        let unit = render_unit(&ctx_with(
            "/home/john doe/bin/kb-mcp",
            "/home/john doe/.config/kb-mcp/kb-mcp",
        ))
        .unwrap();
        assert!(
            unit.contains("ExecStart=\"/home/john doe/bin/kb-mcp\" serve"),
            "binary path was not quoted: {unit}"
        );
        // WorkingDirectory で quote が解釈されるかは未確認なので素のまま出す。
        assert!(
            unit.contains("WorkingDirectory=/home/john doe/.config/kb-mcp/kb-mcp"),
            "working directory should be emitted verbatim: {unit}"
        );
    }

    // ---- render_plist ----

    #[test]
    fn a_plain_plist_is_rendered_verbatim() {
        let plist = render_plist(&ctx_with(
            "/Users/me/.cargo/bin/kb-mcp",
            "/Users/me/Library/Application Support/kb-mcp/kb-mcp",
        ));
        assert!(plist.contains("<string>com.kb-mcp.kb-mcp</string>"));
        assert!(plist.contains("<string>/Users/me/.cargo/bin/kb-mcp</string>"));
        assert!(
            plist.contains("<string>/Users/me/Library/Application Support/kb-mcp/kb-mcp</string>")
        );
    }

    #[test]
    fn xml_metacharacters_in_a_path_are_escaped_in_the_plist() {
        let plist = render_plist(&ctx_with("/Users/a&b/bin/kb-mcp", "/Users/a&b/<cfg>"));
        assert!(
            plist.contains("<string>/Users/a&amp;b/bin/kb-mcp</string>"),
            "binary path was not escaped: {plist}"
        );
        assert!(
            plist.contains("<string>/Users/a&amp;b/&lt;cfg&gt;</string>"),
            "config home was not escaped: {plist}"
        );
        assert!(
            !plist.contains("a&b"),
            "a raw ampersand survived into the plist: {plist}"
        );
    }

    #[test]
    fn the_plist_makes_launchd_create_the_logs_private() {
        let plist = render_plist(&ctx_with("/Users/me/bin/kb-mcp", "/Users/me/cfg"));
        // launchd creates these files, before exec and with the job's umask, so
        // this key is the only thing standing between the daemon's stderr and
        // mode 0644. A string with a leading zero is octal per launchd.plist(5);
        // an <integer> would be read as decimal (BU-24).
        assert!(
            plist.contains("<key>Umask</key>\n    <string>0077</string>"),
            "the plist must set Umask, or launchd creates the logs world-readable: {plist}"
        );
        assert!(
            !plist.contains("<key>Umask</key>\n    <integer>"),
            "an <integer> Umask is read as decimal, not octal: {plist}"
        );
        assert!(
            plist.contains("<string>/Users/me/cfg/kb-mcp.out</string>")
                && plist.contains("<string>/Users/me/cfg/kb-mcp.err</string>"),
            "the log paths must stay the ones macos::tighten_existing_logs \
             re-chmods: {plist}"
        );
    }

    // ---- --config on the launch line (BU-07) ----

    /// The daemon starts with its working directory set to the config home, so
    /// it *finds* `kb-mcp.toml` either way. Naming it is what makes the source
    /// `Explicit` — and therefore Trusted — no matter what `KB_MCP_CONFIG_HOME`
    /// was set to at install time and no longer is at run time.
    #[test]
    fn both_unit_and_plist_name_the_config_file_the_installer_wrote() {
        let ctx = ctx_with("/home/u/.cargo/bin/kb-mcp", "/home/u/.config/kb-mcp/kb-mcp");

        let unit = render_unit(&ctx).unwrap();
        assert!(
            unit.contains(
                "ExecStart=/home/u/.cargo/bin/kb-mcp serve \
                 --config /home/u/.config/kb-mcp/kb-mcp/kb-mcp.toml"
            ),
            "the unit must name the config file: {unit}"
        );

        let plist = render_plist(&ctx);
        assert!(
            plist.contains(
                "<string>--config</string>\n        \
                 <string>/home/u/.config/kb-mcp/kb-mcp/kb-mcp.toml</string>"
            ),
            "the flag and its value are separate array elements, or launchd \
             passes one argument containing a space: {plist}"
        );

        // Whatever the installer writes and whatever the launch line reads must
        // be the same name — see SERVICE_CONFIG_FILE.
        assert!(unit.contains(SERVICE_CONFIG_FILE) && plist.contains(SERVICE_CONFIG_FILE));
    }

    /// `ExecStart` is split on whitespace and `%` is a systemd specifier, so the
    /// config path needs exactly the treatment the binary path already gets.
    /// Without it a home directory with a space silently truncates the value.
    #[test]
    fn the_config_argument_is_quoted_and_percent_escaped_like_the_binary() {
        let unit = render_unit(&ctx_with(
            "/home/john doe/bin/kb-mcp",
            "/home/john doe/.config/kb-mcp/kb-mcp",
        ))
        .unwrap();
        assert!(
            unit.contains("--config \"/home/john doe/.config/kb-mcp/kb-mcp/kb-mcp.toml\""),
            "a space in the config path must not split the argument: {unit}"
        );

        let unit = render_unit(&ctx_with("/opt/kb-mcp", "/srv/100%/cfg")).unwrap();
        assert!(
            unit.contains("--config /srv/100%%/cfg/kb-mcp.toml"),
            "specifier expansion runs before unquoting, so `%` must be doubled: {unit}"
        );
    }

    /// The plist embeds the path in XML, where `&` is fatal and legal in a
    /// macOS path — the same reason the binary path is escaped.
    #[test]
    fn xml_metacharacters_in_the_config_path_are_escaped_too() {
        let plist = render_plist(&ctx_with("/Users/me/bin/kb-mcp", "/Users/a&b/cfg"));
        assert!(
            plist.contains("<string>/Users/a&amp;b/cfg/kb-mcp.toml</string>"),
            "config path was not escaped: {plist}"
        );
        assert!(
            !plist.contains("a&b"),
            "a raw ampersand survived into the plist: {plist}"
        );
    }

    #[test]
    fn no_auto_start_emits_false_for_run_at_load_and_keep_alive() {
        let mut ctx = ctx_with("/Users/me/bin/kb-mcp", "/Users/me/cfg");
        ctx.auto_start = false;
        let plist = render_plist(&ctx);
        assert!(plist.contains("<key>RunAtLoad</key>\n    <false/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n    <false/>"));
    }
}
