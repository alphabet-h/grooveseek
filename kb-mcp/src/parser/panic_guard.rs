//! parser の panic を per-file の `Err` に正規化するための共有機構
//! (full-audit 2026-07-26 AU-21)。
//!
//! # なぜ必要か
//!
//! `kb-mcp index` は 1 ファイルずつ parse する。parse が `Err` を返した場合は
//! `indexer.rs` が per-file skip (warn + 次のファイルへ) するが、**panic は
//! この `match` を巻き戻して通り抜ける**。indexer は逐次実行なので、malformed
//! な 1 ファイルの panic がそのまま `index` 実行全体を落とす — 数千ファイルの
//! KB で 1 個の壊れた docx があるだけで再 index が完了しなくなる。
//!
//! panic は「起きないはず」の事象だが、parser は **信頼できない外部入力**
//! (ユーザの KB に置かれた任意のバイナリ) を crate 群 (calamine / zip /
//! quick-xml / oxidize-pdf) に食わせる。これらの crate 内の整数演算や
//! slice index 由来の panic を我々が事前に全て潰すことはできないため、
//! **境界で catch して `Err` に正規化する** のが唯一実用的な防御になる。
//!
//! # 出力抑止 (thread-local flag + once-install hook)
//!
//! catch するだけでは default panic hook が backtrace を stderr に吐くため、
//! 壊れたファイルが混ざるたびに `index` の出力が backtrace で埋まる。
//! [`SuppressPanicOutputGuard`] が立てる thread-local flag を、
//! [`install_panic_hook_once`] が一度だけ install する wrapper hook が参照し、
//! **その panic を起こしたスレッド自身が parse 中である場合だけ** 出力を
//! 抑制する。panic の内容 (payload) は捨てずに `Err` のメッセージへ載せる
//! ので、情報自体は失われない。
//!
//! この方式は元々 `pdf.rs` にあったもの (PR #69 round 2 の codex P2 対応:
//! hook を毎回 swap する旧実装は並行抽出で race する) を、docx/xlsx/pptx を
//! 含む全 parser で共有するためにここへ移設した。

use std::any::Any;
use std::cell::Cell;
use std::sync::Once;

use anyhow::{Result, anyhow};

thread_local! {
    /// parse 中 (= [`catch_parser_panic`] の `catch_unwind` 内) は true。
    /// process-global な panic hook が「この panic はこのスレッドの parse 由来
    /// だから backtrace 出力を抑制すべきか」を判定するために読む。
    pub(crate) static SUPPRESS_PANIC_OUTPUT: Cell<bool> = const { Cell::new(false) };
}

/// panic hook を once-install するためのフラグ。
static INSTALL_PANIC_HOOK: Once = Once::new();

/// panic hook を **一度だけ** インストールする。
///
/// 旧実装 (`pdf.rs`) は抽出の呼び出し毎に `take_hook`/`set_hook` で
/// process-global な hook を no-op ↔ 元の hook に swap していた。これは
/// 並行 (例: HTTP `get_document` からの複数リクエスト) に複数スレッドが
/// parse すると race する (codex P2, PR #69 round 2): (1) 片方の parse が
/// 終わって `set_hook(prev_hook)` で「元の hook」として復元する値が、実は
/// 他方が仕込んだ no-op hook だった場合、以後プロセス全体で panic 報告が
/// 恒久的に無効化される。(2) parse 中 (no-op hook が刺さっている間) は
/// parse と無関係な他スレッドの panic まで一緒に隠れてしまう。
///
/// 現方式: wrapper hook を最初の parse 時に一度だけ install し、以後は
/// **二度と入れ替えない**。wrapper は [`SUPPRESS_PANIC_OUTPUT`]
/// (thread-local) を見て、抽出中の **その panic を起こしたスレッド自身**
/// の出力だけを抑制し、他スレッドの panic は常に元の hook (`prev`) に
/// そのまま委譲する。
pub(crate) fn install_panic_hook_once() {
    INSTALL_PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let suppress = SUPPRESS_PANIC_OUTPUT.with(Cell::get);
            if !suppress {
                prev(info);
            }
        }));
    });
}

/// [`SUPPRESS_PANIC_OUTPUT`] (thread-local) を true にする RAII guard。
/// Drop で **construct 時の値** に戻す — `catch_unwind` 内で実際に panic して
/// unwind される場合も Drop は呼ばれるため、"panic した後ずっと抑制された
/// まま" になることはない
/// (`pdf.rs::test_suppress_panic_output_guard_resets_flag_even_on_panic`)。
///
/// 「false に戻す」ではなく「元の値に戻す」なのは **re-entrant** にするため。
/// parser の内部がさらに [`catch_parser_panic`] を呼ぶ (= guard が入れ子に
/// なる) 場合、内側の Drop が無条件に false を書くと、外側がまだ生きている
/// のに抑制が解けてしまう。
pub(crate) struct SuppressPanicOutputGuard {
    prev: bool,
}

impl SuppressPanicOutputGuard {
    pub(crate) fn new() -> Self {
        let prev = SUPPRESS_PANIC_OUTPUT.with(|flag| flag.replace(true));
        Self { prev }
    }
}

impl Drop for SuppressPanicOutputGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        SUPPRESS_PANIC_OUTPUT.with(|flag| flag.set(prev));
    }
}

/// `f` の実行中に起きた panic を `Err` に正規化する。panic 以外の挙動
/// (`Ok` / `Err`) はそのまま透過する。
///
/// - `path_hint` — `kb_path` 相対パス。どのファイルで落ちたかを示す
/// - `stage` — parser id (`"docx"` / `"pdf"` 等)。エラーメッセージ用
///
/// panic payload (`panic!("...")` の文字列) はメッセージに載せる。catch と
/// 同時に stderr への backtrace 出力は抑制される (モジュール doc 参照) ため、
/// payload を落とすと「何が起きたか」の手掛かりが完全に消えてしまう。
pub(crate) fn catch_parser_panic<T>(
    path_hint: &str,
    stage: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    install_panic_hook_once();
    let guard = SuppressPanicOutputGuard::new();
    // AssertUnwindSafe の根拠: `f` が触る状態は「parser 自身 (stateless な
    // unit struct)」「入力バイト列 (&[u8]、読み取り専用)」「クロージャ内で
    // 完結する zip / XML reader」だけで、panic 時にはそれら全てを破棄して
    // `Err` を返す。呼び出し側から観測できる「壊れかけた状態」は残らない。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    drop(guard); // parse 終了 (成功 / panic 問わず) 後は即座に抑制解除

    match result {
        Ok(inner) => inner,
        Err(payload) => Err(anyhow!(
            "{path_hint}: {stage} parser panicked: {}",
            panic_payload_message(payload.as_ref())
        )),
    }
}

/// panic payload (`Box<dyn Any + Send>`) から人間可読なメッセージを取り出す。
/// `panic!("literal")` は `&'static str`、`panic!("{x}")` や
/// `assert!`/整数 overflow 等は `String` になる。どちらでもない payload
/// (`panic_any(42)` 等) は型情報が無いので固定文言にフォールバックする。
fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catch_parser_panic_passes_through_ok() {
        let got = catch_parser_panic("docs/a.docx", "docx", || Ok(42)).unwrap();
        assert_eq!(got, 42);
    }

    #[test]
    fn test_catch_parser_panic_passes_through_err() {
        let err = catch_parser_panic::<()>("docs/a.docx", "docx", || {
            Err(anyhow!("cannot open docx zip"))
        })
        .expect_err("Err must pass through unchanged");
        assert!(err.to_string().contains("cannot open docx zip"));
    }

    #[test]
    fn test_catch_parser_panic_converts_panic_to_err() {
        // AU-21 の核: parser 内の panic が呼び出し側に伝播せず Err になる。
        let err = catch_parser_panic::<()>("docs/broken.xlsx", "xlsx", || {
            panic!("attempt to subtract with overflow");
        })
        .expect_err("a panicking parser must yield Err, not unwind to the caller");
        let msg = err.to_string();
        assert!(msg.contains("docs/broken.xlsx"), "got: {msg}");
        assert!(msg.contains("xlsx parser panicked"), "got: {msg}");
        assert!(
            msg.contains("attempt to subtract with overflow"),
            "panic payload must be preserved in the error message, got: {msg}"
        );
    }

    #[test]
    fn test_catch_parser_panic_converts_formatted_panic_to_err() {
        // `panic!("{}", x)` の payload は `&'static str` ではなく `String`。
        // 両方の downcast 経路を押さえる。
        let n = 7;
        let err = catch_parser_panic::<()>("docs/broken.pptx", "pptx", || {
            panic!("slide {n} is malformed");
        })
        .expect_err("panic must yield Err");
        assert!(
            err.to_string().contains("slide 7 is malformed"),
            "got: {err}"
        );
    }

    #[test]
    fn test_catch_parser_panic_handles_non_string_payload() {
        let err = catch_parser_panic::<()>("docs/x.docx", "docx", || {
            std::panic::panic_any(42u32);
        })
        .expect_err("panic must yield Err even for a non-string payload");
        assert!(
            err.to_string().contains("non-string panic payload"),
            "got: {err}"
        );
    }

    #[test]
    fn test_catch_parser_panic_resets_suppress_flag() {
        assert!(
            !SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "flag must start false"
        );
        let _ = catch_parser_panic::<()>("x.docx", "docx", || panic!("boom"));
        assert!(
            !SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "flag must be reset after a panicking parse, otherwise every later \
             panic on this thread stays silent"
        );
    }

    #[test]
    fn test_suppress_panic_output_guard_is_reentrant() {
        // 入れ子の guard: 内側の Drop で抑制が解けてはいけない (外側の
        // catch_unwind がまだ生きているため)。
        let outer = SuppressPanicOutputGuard::new();
        {
            let _inner = SuppressPanicOutputGuard::new();
            assert!(SUPPRESS_PANIC_OUTPUT.with(Cell::get));
        }
        assert!(
            SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "dropping the inner guard must not clear suppression while the outer \
             guard is still alive"
        );
        drop(outer);
        assert!(
            !SUPPRESS_PANIC_OUTPUT.with(Cell::get),
            "dropping the outermost guard must restore the original value"
        );
    }
}
