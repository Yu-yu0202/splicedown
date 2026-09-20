use anstyle::{AnsiColor, Style};
use clap::Parser;
use std::path::PathBuf;

use crate::preset::ExcludePreset;

fn about() -> String {
    let style = Style::new().bold().fg_color(Some(AnsiColor::Green.into()));

    style.render().to_string()
        + "splicedown"
        + &*style.render_reset().to_string()
        + " - A fast, modern Rust bundler."
}

#[derive(Parser)]
#[command(name = "splicedown", version, about = about())]
pub(crate) struct Cli {
    /// Bundle target entry file
    #[arg(default_value = "src/main.rs", value_name = "FILE")]
    pub(crate) entry: PathBuf,

    /// Path to Cargo.toml (searched upward from entry if omitted)
    #[arg(long, value_name = "PATH")]
    pub(crate) manifest_path: Option<PathBuf>,

    /// Output file (stdout if omitted)
    #[arg(short, long, value_name = "PATH")]
    pub(crate) output: Option<PathBuf>,

    /// Crate(s) excluded from bundling (repeatable)
    #[arg(short, long, value_name = "CRATE")]
    pub(crate) exclude: Vec<String>,

    /// Judge environment preset(s) excluded from bundling (repeatable)
    #[arg(long, value_name = "PRESET")]
    pub(crate) exclude_preset: Vec<ExcludePreset>,

    /// Enable compiler-confirmed dead-code removal (enabled by default)
    #[arg(
        long,
        overrides_with_all = ["no_minify", "no_minify_test"]
    )]
    pub(crate) minify: bool,

    /// Disable minification, including test-only removal unless re-enabled
    /// with --minify-test
    #[arg(long, overrides_with = "minify")]
    pub(crate) no_minify: bool,

    /// Enable removal of test-only items (enabled by default)
    #[arg(long, overrides_with = "no_minify_test")]
    pub(crate) minify_test: bool,

    /// Disable removal of test-only items
    #[arg(long, overrides_with = "minify_test")]
    pub(crate) no_minify_test: bool,

    /// Skip the post-bundle `cargo check`
    #[arg(long)]
    pub(crate) no_check: bool,

    /// Keep the temporary directory used by `cargo check`
    #[arg(long)]
    pub(crate) keep_check_dir: bool,
}

pub(crate) fn parse() -> Cli {
    Cli::parse()
}

impl Cli {
    pub(crate) fn minify_enabled(&self) -> bool {
        self.minify || !self.no_minify
    }

    pub(crate) fn minify_test_enabled(&self) -> bool {
        if self.no_minify {
            self.minify_test
        } else {
            !self.no_minify_test
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_takes_one_value_and_can_precede_entry() {
        let cli = Cli::try_parse_from([
            "splicedown",
            "--exclude",
            "lib-a",
            "src/bin.rs",
            "--exclude",
            "lib-b",
        ])
        .unwrap();

        assert_eq!(cli.entry, PathBuf::from("src/bin.rs"));
        assert_eq!(cli.exclude, ["lib-a", "lib-b"]);
    }

    #[test]
    fn exclude_preset_is_repeatable_and_accepts_atcoder_alias() {
        let cli = Cli::try_parse_from([
            "splicedown",
            "--exclude-preset",
            "atcoder-2025",
            "--exclude-preset",
            "atcoder-2025-10",
        ])
        .unwrap();

        assert_eq!(
            cli.exclude_preset,
            [
                ExcludePreset::Atcoder2025October,
                ExcludePreset::Atcoder2025October
            ]
        );
    }

    #[test]
    fn minification_is_enabled_by_default_and_can_be_disabled_independently() {
        let default = Cli::try_parse_from(["splicedown"]).unwrap();
        let no_dead = Cli::try_parse_from(["splicedown", "--no-minify"]).unwrap();
        let no_tests = Cli::try_parse_from(["splicedown", "--no-minify-test"]).unwrap();
        let neither =
            Cli::try_parse_from(["splicedown", "--no-minify", "--no-minify-test"]).unwrap();

        assert!(default.minify_enabled());
        assert!(default.minify_test_enabled());
        assert!(!no_dead.minify_enabled());
        assert!(!no_dead.minify_test_enabled());
        assert!(no_tests.minify_enabled());
        assert!(!no_tests.minify_test_enabled());
        assert!(!neither.minify_enabled());
        assert!(!neither.minify_test_enabled());
    }

    #[test]
    fn explicit_enable_flag_overrides_an_earlier_disable_flag() {
        let cli = Cli::try_parse_from([
            "splicedown",
            "--no-minify",
            "--minify",
            "--no-minify-test",
            "--minify-test",
        ])
        .unwrap();

        assert!(cli.minify_enabled());
        assert!(cli.minify_test_enabled());
    }

    #[test]
    fn minify_enables_test_minification_but_test_minification_is_standalone() {
        let minify_last =
            Cli::try_parse_from(["splicedown", "--no-minify-test", "--minify"]).unwrap();
        assert!(minify_last.minify_enabled());
        assert!(minify_last.minify_test_enabled());

        let test_only =
            Cli::try_parse_from(["splicedown", "--no-minify", "--minify-test"]).unwrap();
        assert!(!test_only.minify_enabled());
        assert!(test_only.minify_test_enabled());

        let no_tests_last =
            Cli::try_parse_from(["splicedown", "--minify", "--no-minify-test"]).unwrap();
        assert!(no_tests_last.minify_enabled());
        assert!(!no_tests_last.minify_test_enabled());
    }
}
