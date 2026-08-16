//! Status / list for groove service backends.
//! Phase 1: OS native + toml fallback. HTTP enrichment (`uptime_secs`, `model`) is deferred to Phase 2.
use crate::service::{ServiceState, backend, resolve_config_home, validate_service_name};
use anyhow::{Result, anyhow};
use std::path::PathBuf;

pub fn run_status(service_name: &str) -> Result<String> {
    let name = validate_service_name(service_name).map_err(|e| anyhow!(e))?;
    let state = backend().status(&name)?;
    let state = enrich_with_toml(&name, state);
    Ok(format_state(&name, &state))
}

/// `run_list` の見出し行。`format_row` の桁揃えはこの見出しに対して定義される
/// ので、片方だけ動かすと表が崩れる。両方から参照できるよう const にしてある。
const LIST_HEADER: &str =
    "NAME       STATUS     BIND               KB_PATH                UPTIME\n";

pub fn run_list() -> Result<String> {
    let entries = backend().list()?;
    let mut s = String::from(LIST_HEADER);
    for (name, state) in entries {
        let state = enrich_with_toml(&name, state);
        s.push_str(&format_row(&name, &state));
        s.push('\n');
    }
    Ok(s)
}

fn enrich_with_toml(name: &str, state: ServiceState) -> ServiceState {
    let toml_src = resolve_config_home(name)
        .ok()
        .map(|h| h.join("groove.toml"))
        .and_then(|p| std::fs::read_to_string(&p).ok());
    enrich_from_toml_str(state, toml_src.as_deref())
}

/// `enrich_with_toml` の純粋な半分。読み込み済みの `groove.toml` から
/// `[transport.http].bind` と `kb_path` を拾い、`state` の**欠けている**
/// フィールドだけを埋める。OS が報告した値は常に優先される (= toml は
/// installed-but-not-running のときに空欄を埋めるための補助)。
///
/// ファイルが無い / 読めない / TOML として壊れている場合はいずれも
/// 「上書きしない」に倒す。status 表示のために起動失敗させる意味がないため。
fn enrich_from_toml_str(state: ServiceState, toml_src: Option<&str>) -> ServiceState {
    let (bind_toml, kb_toml) = toml_src
        .and_then(|c| toml::from_str::<toml::Value>(c).ok())
        .map(|v| {
            let bind = v
                .get("transport")
                .and_then(|t| t.get("http"))
                .and_then(|h| h.get("bind"))
                .and_then(|b| b.as_str())
                .map(String::from);
            let kb = v.get("kb_path").and_then(|p| p.as_str()).map(PathBuf::from);
            (bind, kb)
        })
        .unwrap_or((None, None));
    match state {
        ServiceState::Running {
            uptime_secs,
            model,
            bind,
            kb_path,
        } => ServiceState::Running {
            uptime_secs,
            bind: bind.or(bind_toml),
            kb_path: kb_path.or(kb_toml),
            model,
        },
        ServiceState::Stopped { bind, kb_path } => ServiceState::Stopped {
            bind: bind.or(bind_toml),
            kb_path: kb_path.or(kb_toml),
        },
        s => s,
    }
}

fn format_state(name: &str, state: &ServiceState) -> String {
    match state {
        ServiceState::Running {
            uptime_secs,
            bind,
            kb_path,
            model,
        } => format!(
            "{}: running (uptime {}s, bind {}, kb_path {}, model {})",
            name,
            uptime_secs,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
            model.as_deref().unwrap_or("(unknown)"),
        ),
        ServiceState::Stopped { bind, kb_path } => format!(
            "{}: stopped (bind {}, kb_path {})",
            name,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
        ),
        ServiceState::NotFound => format!("{}: not found", name),
    }
}

fn format_row(name: &str, state: &ServiceState) -> String {
    match state {
        ServiceState::Running {
            uptime_secs,
            bind,
            kb_path,
            ..
        } => format!(
            "{:<10} running    {:<18} {:<22} {}s",
            name,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
            uptime_secs,
        ),
        ServiceState::Stopped { bind, kb_path } => format!(
            "{:<10} stopped    {:<18} {:<22} -",
            name,
            bind.as_deref().unwrap_or("(unknown)"),
            kb_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(unknown)".into()),
        ),
        ServiceState::NotFound => format!("{:<10} not-found", name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[transport.http].bind` と `kb_path` の両方を持つ、ごく普通の config。
    /// インデント付き multi-line 文字列リテラルは先頭空白を巻き込むので
    /// `concat!` で組む (CLAUDE.local.md の文字列リテラル継続の注意)。
    const FULL_TOML: &str = concat!(
        "kb_path = \"/srv/kb\"\n",
        "[transport.http]\n",
        "bind = \"0.0.0.0:3100\"\n",
    );

    fn running(bind: Option<&str>, kb_path: Option<&str>) -> ServiceState {
        ServiceState::Running {
            uptime_secs: 42,
            bind: bind.map(String::from),
            kb_path: kb_path.map(PathBuf::from),
            model: Some("bge-small-en-v1.5".into()),
        }
    }

    fn stopped(bind: Option<&str>, kb_path: Option<&str>) -> ServiceState {
        ServiceState::Stopped {
            bind: bind.map(String::from),
            kb_path: kb_path.map(PathBuf::from),
        }
    }

    // ---- enrich_from_toml_str: どのフィールドが埋まるか ----

    #[test]
    fn toml_fills_the_gaps_a_running_service_left() {
        let out = enrich_from_toml_str(running(None, None), Some(FULL_TOML));
        assert_eq!(
            out,
            ServiceState::Running {
                uptime_secs: 42,
                bind: Some("0.0.0.0:3100".into()),
                kb_path: Some(PathBuf::from("/srv/kb")),
                model: Some("bge-small-en-v1.5".into()),
            }
        );
    }

    #[test]
    fn toml_fills_the_gaps_a_stopped_service_left() {
        let out = enrich_from_toml_str(stopped(None, None), Some(FULL_TOML));
        assert_eq!(
            out,
            ServiceState::Stopped {
                bind: Some("0.0.0.0:3100".into()),
                kb_path: Some(PathBuf::from("/srv/kb")),
            }
        );
    }

    #[test]
    fn os_reported_values_beat_the_toml() {
        // OS 側が答えられたものは toml で上書きしない。config を編集した後
        // 再起動していない service で、表示が実態から乖離するのを防ぐため。
        let out = enrich_from_toml_str(
            running(Some("127.0.0.1:9999"), Some("/live/kb")),
            Some(FULL_TOML),
        );
        assert_eq!(
            out,
            ServiceState::Running {
                uptime_secs: 42,
                bind: Some("127.0.0.1:9999".into()),
                kb_path: Some(PathBuf::from("/live/kb")),
                model: Some("bge-small-en-v1.5".into()),
            }
        );
    }

    #[test]
    fn each_field_falls_back_independently() {
        // bind は OS から取れて kb_path は取れなかった、という混在。
        let out = enrich_from_toml_str(running(Some("127.0.0.1:9999"), None), Some(FULL_TOML));
        assert_eq!(
            out,
            ServiceState::Running {
                uptime_secs: 42,
                bind: Some("127.0.0.1:9999".into()),
                kb_path: Some(PathBuf::from("/srv/kb")),
                model: Some("bge-small-en-v1.5".into()),
            }
        );
    }

    #[test]
    fn not_found_is_passed_through_untouched() {
        // toml に何が書いてあっても NotFound は NotFound。
        assert_eq!(
            enrich_from_toml_str(ServiceState::NotFound, Some(FULL_TOML)),
            ServiceState::NotFound
        );
    }

    // ---- enrich_from_toml_str: 読めない toml をどう扱うか ----

    #[test]
    fn a_missing_config_file_leaves_the_state_alone() {
        assert_eq!(
            enrich_from_toml_str(running(None, None), None),
            running(None, None)
        );
    }

    #[test]
    fn malformed_toml_is_ignored_rather_than_fatal() {
        // status 表示のために起動失敗させる意味はないので、パースエラーは
        // 「上書きしない」に倒す。
        assert_eq!(
            enrich_from_toml_str(running(None, None), Some("this is not = = toml")),
            running(None, None)
        );
    }

    #[test]
    fn a_toml_without_the_relevant_keys_changes_nothing() {
        let unrelated = concat!(
            "model = \"bge-m3\"\n",
            "[transport.stdio]\n",
            "enabled = true\n",
        );
        assert_eq!(
            enrich_from_toml_str(stopped(None, None), Some(unrelated)),
            stopped(None, None)
        );
    }

    #[test]
    fn non_string_values_are_ignored() {
        // `bind = 3100` (integer) のような型違いは as_str() が None を返す。
        let wrong_types = concat!("kb_path = 42\n", "[transport.http]\n", "bind = 3100\n",);
        assert_eq!(
            enrich_from_toml_str(stopped(None, None), Some(wrong_types)),
            stopped(None, None)
        );
    }

    // ---- format_state: 3 arm + 欠損時のプレースホルダ ----

    #[test]
    fn format_state_renders_a_running_service() {
        assert_eq!(
            format_state("kb", &running(Some("127.0.0.1:3100"), Some("/srv/kb"))),
            "kb: running (uptime 42s, bind 127.0.0.1:3100, kb_path /srv/kb, model bge-small-en-v1.5)"
        );
    }

    #[test]
    fn format_state_renders_a_stopped_service() {
        assert_eq!(
            format_state("kb", &stopped(Some("127.0.0.1:3100"), Some("/srv/kb"))),
            "kb: stopped (bind 127.0.0.1:3100, kb_path /srv/kb)"
        );
    }

    #[test]
    fn format_state_renders_not_found() {
        assert_eq!(format_state("kb", &ServiceState::NotFound), "kb: not found");
    }

    #[test]
    fn format_state_marks_every_missing_field_unknown() {
        // toml も無く OS も答えられなかったとき、空欄ではなく (unknown) と出す。
        assert_eq!(
            format_state(
                "kb",
                &ServiceState::Running {
                    uptime_secs: 0,
                    bind: None,
                    kb_path: None,
                    model: None,
                }
            ),
            "kb: running (uptime 0s, bind (unknown), kb_path (unknown), model (unknown))"
        );
        assert_eq!(
            format_state("kb", &stopped(None, None)),
            "kb: stopped (bind (unknown), kb_path (unknown))"
        );
    }

    // ---- format_row: 3 arm + 見出しとの桁揃え ----

    #[test]
    fn format_row_columns_line_up_with_the_list_header() {
        // `run_list` は LIST_HEADER の下に format_row を並べる。どちらかの
        // 幅だけを触ると表が崩れるが、目視でしか気付けない。見出しのラベル
        // 位置と行のフィールド位置が一致することを固定する。
        let status_col = LIST_HEADER.find("STATUS").unwrap();
        let bind_col = LIST_HEADER.find("BIND").unwrap();
        let kb_col = LIST_HEADER.find("KB_PATH").unwrap();
        let uptime_col = LIST_HEADER.find("UPTIME").unwrap();

        let row = format_row("kb", &running(Some("127.0.0.1:3100"), Some("/srv/kb")));
        assert_eq!(row.find("running").unwrap(), status_col);
        assert_eq!(row.find("127.0.0.1:3100").unwrap(), bind_col);
        assert_eq!(row.find("/srv/kb").unwrap(), kb_col);
        assert_eq!(row.find("42s").unwrap(), uptime_col);

        let row = format_row("kb", &stopped(Some("127.0.0.1:3100"), Some("/srv/kb")));
        assert_eq!(row.find("stopped").unwrap(), status_col);
        assert_eq!(row.find("127.0.0.1:3100").unwrap(), bind_col);
        assert_eq!(row.find("/srv/kb").unwrap(), kb_col);
        assert_eq!(row.rfind('-').unwrap(), uptime_col);
    }

    #[test]
    fn format_row_renders_not_found_without_trailing_columns() {
        let row = format_row("kb", &ServiceState::NotFound);
        assert_eq!(
            row.find("not-found").unwrap(),
            LIST_HEADER.find("STATUS").unwrap()
        );
        assert_eq!(row, "kb         not-found");
    }

    #[test]
    fn format_row_substitutes_unknown_for_missing_fields() {
        let row = format_row("kb", &stopped(None, None));
        assert_eq!(row.matches("(unknown)").count(), 2);
        assert_eq!(
            row.find("(unknown)").unwrap(),
            LIST_HEADER.find("BIND").unwrap()
        );
    }

    #[test]
    fn a_long_name_pushes_the_columns_right_rather_than_being_truncated() {
        // `{:<10}` は下限幅であって上限ではない。10 文字を超える service 名は
        // 切り詰められず、その行だけ桁がずれる。表示が崩れても名前が読める方を
        // 選んでいる、という現状仕様を固定する。
        let long = "a-very-long-service-name";
        let row = format_row(long, &stopped(Some("127.0.0.1:3100"), None));
        assert!(row.starts_with(long), "name must survive intact: {row}");
        assert!(row.find("stopped").unwrap() > LIST_HEADER.find("STATUS").unwrap());
    }
}
