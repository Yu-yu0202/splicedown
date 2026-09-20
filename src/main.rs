use anyhow::Result;
use tracing::error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .compact()
        .init();

    if let Err(e) = splicedown_rs::run().await {
        error!("{e:#}");
        std::process::exit(1);
    } else {
        Ok(())
    }
}
