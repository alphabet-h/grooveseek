//! Parser registry. Owns a set of `Box<dyn Parser>` keyed by lowercase
//! extension. Shared across indexer / server / future watcher.

use anyhow::Result;

use super::{MarkdownParser, Parser, TxtParser};

pub struct Registry {
    parsers: Vec<Box<dyn Parser>>,
}

impl Registry {
    /// Build a Registry from a list of parser ids (from `[parsers].enabled`).
    /// Unknown ids fail loudly — this catches typos (`"markdown"` instead of
    /// `"md"`) and parsers that don't exist yet (`"pdf"` / `"rst"` / `"adoc"`).
    pub fn from_enabled(ids: &[String]) -> Result<Self> {
        if ids.is_empty() {
            anyhow::bail!("[parsers].enabled must contain at least one id (got empty list)");
        }
        let mut parsers: Vec<Box<dyn Parser>> = Vec::with_capacity(ids.len());
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for id in ids {
            let lower = id.to_ascii_lowercase();
            if !seen.insert(lower.clone()) {
                anyhow::bail!("[parsers].enabled contains duplicate id {:?}", id);
            }
            let parser: Box<dyn Parser> = match lower.as_str() {
                "md" => Box::new(MarkdownParser),
                "txt" => Box::new(TxtParser),
                other => anyhow::bail!(
                    "[parsers].enabled contains unknown id {:?} — \
                     supported in this build: md, txt",
                    other
                ),
            };
            parsers.push(parser);
        }
        Ok(Self { parsers })
    }

    /// Default registry: `["md"]` only. Pre-feature-20 behaviour — `.txt`
    /// support is opt-in via `kb-mcp.toml` `[parsers].enabled = ["md", "txt"]`.
    pub fn defaults() -> Self {
        Self {
            parsers: vec![Box::new(MarkdownParser)],
        }
    }

    /// Lookup a parser by file extension (lowercase, no leading dot).
    /// Case-insensitive match.
    pub fn by_extension(&self, ext: &str) -> Option<&dyn Parser> {
        self.parsers
            .iter()
            .find(|p| p.extension().eq_ignore_ascii_case(ext))
            .map(|b| b.as_ref())
    }

    /// All enabled extensions, used by `walkdir` filtering and by the
    /// (future) file watcher to limit fsnotify events.
    pub fn extensions(&self) -> Vec<&'static str> {
        self.parsers.iter().map(|p| p.extension()).collect()
    }

    /// True if `ext` (without leading dot, lowercase recommended) is registered.
    pub fn has_extension(&self, ext: &str) -> bool {
        self.parsers.iter().any(|p| p.extension() == ext)
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::defaults()
    }
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("extensions", &self.extensions())
            .finish()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_is_md_only() {
        let r = Registry::defaults();
        assert_eq!(r.extensions(), vec!["md"]);
        assert!(r.by_extension("md").is_some());
        assert!(r.by_extension("txt").is_none());
    }

    #[test]
    fn test_from_enabled_md_and_txt() {
        let r = Registry::from_enabled(&["md".into(), "txt".into()]).unwrap();
        let exts = r.extensions();
        assert!(exts.contains(&"md"));
        assert!(exts.contains(&"txt"));
        assert!(r.by_extension("MD").is_some(), "should be case-insensitive");
        assert!(r.by_extension("TXT").is_some());
    }

    #[test]
    fn test_from_enabled_rejects_empty() {
        let err = Registry::from_enabled(&[]).expect_err("empty must fail");
        assert!(err.to_string().contains("at least one id"));
    }

    #[test]
    fn test_from_enabled_rejects_unknown() {
        let err = Registry::from_enabled(&["pdf".into()]).expect_err("unknown id must fail");
        let msg = err.to_string();
        assert!(msg.contains("pdf"));
        assert!(msg.contains("supported"));
    }

    #[test]
    fn test_from_enabled_rejects_duplicates() {
        let err = Registry::from_enabled(&["md".into(), "MD".into()])
            .expect_err("case-insensitive duplicate must fail");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn test_from_enabled_case_insensitive_id() {
        // "MD" in config normalises to "md" — both accepted
        let r = Registry::from_enabled(&["MD".into()]).unwrap();
        assert_eq!(r.extensions(), vec!["md"]);
    }
}
