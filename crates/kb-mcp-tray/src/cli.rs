#![cfg(target_os = "windows")]

use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "kb-mcp-tray",
    version,
    about = "kb-mcp daemon tray monitor (Windows)"
)]
pub struct Cli {
    /// Service name (matches `--service-name` passed to `kb-mcp service install`)
    #[arg(long, default_value = "kb-mcp")]
    pub service_name: String,

    /// Override config home discovery (rare, opt-in for testing)
    #[arg(long)]
    pub kb_path: Option<PathBuf>,

    /// Attach a console for debugging (release builds otherwise hide stdio)
    #[arg(long)]
    pub debug: bool,
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_kb_mcp_service() {
        let cli = Cli::try_parse_from(["kb-mcp-tray"]).unwrap();
        assert_eq!(cli.service_name, "kb-mcp");
        assert!(!cli.debug);
        assert!(cli.kb_path.is_none());
    }

    #[test]
    fn parses_service_name_override() {
        let cli = Cli::try_parse_from(["kb-mcp-tray", "--service-name", "work"]).unwrap();
        assert_eq!(cli.service_name, "work");
    }

    #[test]
    fn parses_debug_flag() {
        let cli = Cli::try_parse_from(["kb-mcp-tray", "--debug"]).unwrap();
        assert!(cli.debug);
    }
}
