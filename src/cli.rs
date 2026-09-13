use std::path::PathBuf;
use anstyle::{AnsiColor, Style};
use clap::Parser;

fn about() -> String {
    let style = Style::new().bold().fg_color(Some(AnsiColor::Green.into()));
    
    style.render().to_string() + 
        "splicedown" +
        &*style.render_reset().to_string() + 
        " - A fast, modern Rust bundler."
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
    #[arg(short, long, value_name = "CRATE", num_args = 1..)]
    pub(crate) exclude: Vec<String>,

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
