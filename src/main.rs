mod process;
mod logger;
mod storage;

use eyre::Result;
use log::{error, info, warn};
use process::Processor;
use storage::FileStorage;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    logger::init()?;

    info!("Starting Parallel Versioned Storage...");

    let mut storage = FileStorage::new();

    if let Ok(content) = tokio::fs::read_to_string("ingest.txt").await {
        for line in content.lines() {
            let path = line.trim();
            if !path.is_empty() && !path.starts_with('#') {
                storage.add(path).await;
            }
        }
    } else {
        warn!("ingest.txt not found or unreadable, skipping initial ingestion");
    }

    info!("Collected {} unique files", storage.len());

    if let Err(e) = Processor::process_all(storage.paths()).await {
        error!("Fatal error during processing: {:?}", e);
        std::process::exit(1);
    }

    Ok(())
}
