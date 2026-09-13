mod cli;
mod metadata;

use anyhow::Result;

pub async fn run() -> Result<()> {
    cli::parse();
    Ok(())
}
