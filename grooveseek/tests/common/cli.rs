//! Reading the `groove` CLI's answers: the counts `groove status` prints and the JSON
//! `groove search --format json` prints.
//!
//! The caller builds the `groove` command up to the subcommand (the binary, `--config`, any
//! environment it needs), so a test with a real model and one behind
//! [`crate::common::embed_mock`]'s hermetic environment share the same reading
//! ([`crate::common::embed_cli::Fixture`] wraps both).

use super::ansi::strip_ansi;

use std::path::Path;
use std::process::Command;

/// The number on the line of `stdout` that starts with `label` (`"Documents:"`,
/// `"Chunks:"`), as `groove status` prints it.
///
/// The whole rest of the line, trimmed, must be the number: `Documents: 10` reads as 10,
/// never as a 1 that happens to be its first digit, and a line with anything after the
/// number reads as `None`. The first line that starts with `label` decides.
pub fn status_count(stdout: &str, label: &str) -> Option<u64> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix(label))
        .and_then(|n| n.trim().parse().ok())
}

/// `<cmd> status --kb-path <kb>`, which must succeed, and the `label` count it prints
/// ([`status_count`]). `cmd` is `groove` with `--config` and any environment already set.
pub fn run_status_count(mut cmd: Command, kb: &Path, label: &str) -> u64 {
    let out = cmd
        .args(["status", "--kb-path"])
        .arg(kb)
        .output()
        .expect("spawn groove status");
    assert!(
        out.status.success(),
        "groove status failed:\n{}",
        strip_ansi(&String::from_utf8_lossy(&out.stderr))
    );
    status_count(&String::from_utf8_lossy(&out.stdout), label)
        .unwrap_or_else(|| panic!("no {label} line in groove status"))
}

/// `<cmd> search <query> --kb-path <kb> --format json <extra...>`, which must succeed, and
/// the JSON it prints. `cmd` is `groove` with `--config` and any environment already set;
/// `extra` carries the rest (`--model`, `--limit`, filters), in order.
pub fn search_json(mut cmd: Command, kb: &Path, query: &str, extra: &[&str]) -> serde_json::Value {
    let out = cmd
        .args(["search", query, "--kb-path"])
        .arg(kb)
        .args(["--format", "json"])
        .args(extra)
        .output()
        .expect("spawn groove search");
    assert!(
        out.status.success(),
        "groove search failed:\n{}",
        strip_ansi(&String::from_utf8_lossy(&out.stderr))
    );
    serde_json::from_slice(&out.stdout).expect("search stdout is JSON")
}

#[cfg(test)]
mod tests {
    use super::status_count;

    const STATUS: &str = "Documents: 10\nChunks: 37\n";

    #[test]
    fn status_count_reads_the_whole_number() {
        assert_eq!(status_count(STATUS, "Documents:"), Some(10));
        assert_ne!(status_count(STATUS, "Documents:"), Some(1));
        assert_eq!(status_count(STATUS, "Chunks:"), Some(37));
    }

    #[test]
    fn status_count_refuses_a_line_that_is_not_only_a_number() {
        assert_eq!(
            status_count("Documents: 10 (3 skipped)\n", "Documents:"),
            None
        );
        assert_eq!(status_count("Documents: ten\n", "Documents:"), None);
    }

    #[test]
    fn status_count_is_none_without_the_label() {
        assert_eq!(status_count("No index found\n", "Documents:"), None);
        assert_eq!(status_count("", "Chunks:"), None);
    }
}
