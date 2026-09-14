mod cli;
mod metadata;
mod util;

use anyhow::Result;

pub async fn run() -> Result<()> {
    cli::parse();
    Ok(())
}
