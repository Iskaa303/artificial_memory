use crate::storage;
use eyre::Result;
use std::path::PathBuf;

pub async fn process(path: PathBuf) -> Result<()> {
    storage::store_file(&path).await?;
    Ok(())
}
